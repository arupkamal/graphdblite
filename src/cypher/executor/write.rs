//! Write operators — Create, Delete, Set, Remove.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::eval_expr;
use crate::cypher::record::NamedRecord;
use crate::types::*;
use crate::{edge, fts, index, node};

use super::*;

pub(in crate::cypher::executor) fn exec_create_node(
    conn: &Connection,
    labels: &[String],
    alias: Option<&str>,
    properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    let mut props = Properties::new();
    let dummy_rec = NamedRecord::new();
    for (key, expr) in properties {
        let val = eval_expr(expr, &dummy_rec, crate::cypher::eval::EvalCx::new(conn))?;
        if val == Value::Null {
            continue;
        }
        props.insert(key.clone(), val);
    }

    validate_write_labels(labels)?;
    validate_write_properties(&props)?;
    let id = node::create_node(conn, labels, props.clone())?;
    index::update_indexes_for_node(conn, id, labels, None, &props)?;
    fts::update_fts_for_node(conn, id, labels, None, &props)?;

    let mut rec = NamedRecord::new();
    if let Some(alias) = alias {
        rec.set(alias.to_string(), Value::I64(id.0 as i64));
        rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
        // Populate property bindings so the slot bridge can fill prop slots
        // declared in the inferred schema. Deliberately *not* setting
        // `__label`/`__labels` — that pair triggers `build_compound_binding`,
        // and a Variable-resolution shift here would change downstream
        // semantics (e.g. `WITH a, ...` where `a` was previously `Value::I64`
        // but would become `Value::Node`).
        for (k, v) in &props {
            rec.set(format!("{alias}.{k}"), v.clone());
        }
    }
    Ok(vec![rec])
}

pub(in crate::cypher::executor) fn exec_create_edge(
    _conn: &Connection,
    _src_alias: &str,
    _dst_alias: &str,
    _edge_type: &str,
    _properties: &HashMap<String, Expr>,
) -> Result<Vec<NamedRecord>> {
    // Standalone edge creation is handled by exec_create_sequence.
    // This path is only reached for isolated CreateEdge ops (shouldn't happen in practice).
    Ok(vec![])
}

pub(in crate::cypher::executor) fn exec_create_sequence(
    conn: &Connection,
    ops: &[LogicalOp],
) -> Result<Vec<NamedRecord>> {
    // Track variable → NodeId bindings for edge creation.
    let mut bindings: HashMap<String, NodeId> = HashMap::new();
    let mut last_record = NamedRecord::new();

    for op in ops {
        match op {
            LogicalOp::CreateNode {
                labels,
                alias,
                properties,
            } => {
                let mut props = Properties::new();
                for (key, expr) in properties {
                    let val =
                        eval_expr(expr, &last_record, crate::cypher::eval::EvalCx::new(conn))?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                validate_write_labels(labels)?;
                validate_write_properties(&props)?;
                let id = node::create_node(conn, labels, props.clone())?;
                index::update_indexes_for_node(conn, id, labels, None, &props)?;
                fts::update_fts_for_node(conn, id, labels, None, &props)?;
                if let Some(alias) = alias {
                    bindings.insert(alias.clone(), id);
                    last_record.set(alias.clone(), Value::I64(id.0 as i64));
                    last_record.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                    for (k, v) in &props {
                        last_record.set(format!("{alias}.{k}"), v.clone());
                    }
                }
            }
            LogicalOp::CreateEdge {
                src_alias,
                dst_alias,
                edge_type,
                rel_alias,
                properties,
            } => {
                let src = bindings.get(src_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {src_alias}"))
                })?;
                let dst = bindings.get(dst_alias).ok_or_else(|| {
                    GraphError::semantic(format!("unbound variable: {dst_alias}"))
                })?;
                let mut props = Properties::new();
                for (key, expr) in properties {
                    let val =
                        eval_expr(expr, &last_record, crate::cypher::eval::EvalCx::new(conn))?;
                    if val == Value::Null {
                        continue;
                    }
                    props.insert(key.clone(), val);
                }
                validate_write_labels(std::slice::from_ref(edge_type))?;
                validate_write_properties(&props)?;
                let seq = edge::create_edge(conn, *src, *dst, edge_type, props.clone())?;
                // Bind edge metadata for RETURN access.
                if let Some(r_alias) = rel_alias {
                    last_record.set(r_alias.clone(), Value::String(edge_type.clone()));
                    last_record.set(format!("{r_alias}.__src"), Value::I64(src.0 as i64));
                    last_record.set(format!("{r_alias}.__dst"), Value::I64(dst.0 as i64));
                    last_record.set(
                        format!("{r_alias}.__type"),
                        Value::String(edge_type.clone()),
                    );
                    last_record.set(format!("{r_alias}.__edge_seq"), Value::I64(seq as i64));
                    for (key, val) in &props {
                        last_record.set(format!("{r_alias}.{key}"), val.clone());
                    }
                }
            }
            _ => {
                // Shouldn't happen in a CreateSequence.
            }
        }
    }

    Ok(vec![last_record])
}

pub(in crate::cypher::executor) fn exec_match_create(
    conn: &Connection,
    input: &LogicalOp,
    create_ops: &[LogicalOp],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut result = Vec::with_capacity(records.len());

    for rec in &records {
        // Seed bindings from MATCH-bound variables (var → NodeId).
        let mut bindings: HashMap<String, NodeId> = HashMap::new();
        for (key, val) in &rec.fields {
            if !key.contains('.') {
                if let Value::I64(id) = val {
                    bindings.insert(key.clone(), NodeId(*id as u64));
                }
            }
        }

        // Start with a copy of the input record so MATCH-bound vars are available.
        let mut out_rec = rec.clone();

        // Execute each CREATE op using the bindings.
        for op in create_ops {
            match op {
                LogicalOp::CreateNode {
                    labels,
                    alias,
                    properties,
                } => {
                    // Skip if this alias is already bound from the MATCH pipeline.
                    if let Some(a) = alias {
                        if bindings.contains_key(a) {
                            continue;
                        }
                    }
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    validate_write_labels(labels)?;
                    validate_write_properties(&props)?;
                    let id = node::create_node(conn, labels, props.clone())?;
                    index::update_indexes_for_node(conn, id, labels, None, &props)?;
                    fts::update_fts_for_node(conn, id, labels, None, &props)?;
                    if let Some(alias) = alias {
                        bindings.insert(alias.clone(), id);
                        out_rec.set(alias.clone(), Value::I64(id.0 as i64));
                        out_rec.set(format!("{alias}.__id"), Value::I64(id.0 as i64));
                        // Property keys only — see exec_create_node note re:
                        // compound-binding heuristic and __label.
                        for (k, v) in &props {
                            out_rec.set(format!("{alias}.{k}"), v.clone());
                        }
                    }
                }
                LogicalOp::CreateEdge {
                    src_alias,
                    dst_alias,
                    edge_type,
                    rel_alias,
                    properties,
                } => {
                    let src = bindings.get(src_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {src_alias}"))
                    })?;
                    let dst = bindings.get(dst_alias).ok_or_else(|| {
                        GraphError::semantic(format!("unbound variable: {dst_alias}"))
                    })?;
                    let mut props = Properties::new();
                    for (key, expr) in properties {
                        let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                        if val == Value::Null {
                            continue;
                        }
                        props.insert(key.clone(), val);
                    }
                    validate_write_labels(std::slice::from_ref(edge_type))?;
                    validate_write_properties(&props)?;
                    let seq = edge::create_edge(conn, *src, *dst, edge_type, props.clone())?;
                    // Bind edge metadata for RETURN access.
                    if let Some(r_alias) = rel_alias {
                        out_rec.set(r_alias.clone(), Value::String(edge_type.clone()));
                        out_rec.set(format!("{r_alias}.__src"), Value::I64(src.0 as i64));
                        out_rec.set(format!("{r_alias}.__dst"), Value::I64(dst.0 as i64));
                        out_rec.set(
                            format!("{r_alias}.__type"),
                            Value::String(edge_type.clone()),
                        );
                        out_rec.set(format!("{r_alias}.__edge_seq"), Value::I64(seq as i64));
                        for (key, val) in &props {
                            out_rec.set(format!("{r_alias}.{key}"), val.clone());
                        }
                    }
                }
                _ => {}
            }
        }

        result.push(out_rec);
    }

    Ok(result)
}

pub(in crate::cypher::executor) fn exec_delete(
    conn: &Connection,
    input: &LogicalOp,
    exprs: &[Expr],
    detach: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;

    // Two-phase delete: collect all entities first, then delete edges, then nodes.
    // This prevents DeleteConnectedNode errors when multiple paths share nodes.
    let mut edges_to_delete: Vec<(NodeId, NodeId, String, Option<u64>)> = Vec::new();
    let mut nodes_to_delete: Vec<NodeId> = Vec::new();

    for rec in &mut records {
        for expr in exprs {
            if let ExprKind::Variable(var) = &expr.kind {
                collect_var_entities(rec, var, &mut edges_to_delete, &mut nodes_to_delete)?;
                rec.set(format!("{var}.__deleted"), Value::Bool(true));
            } else {
                let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                collect_value_entities(&val, &mut edges_to_delete, &mut nodes_to_delete)?;
            }
        }
    }

    // Phase 1: delete all edges.
    for (src, dst, label, seq) in &edges_to_delete {
        if let Some(s) = seq {
            let _ = edge::delete_single_edge(conn, *src, *dst, label, *s);
        } else {
            let _ = edge::delete_edge(conn, *src, *dst, label);
        }
    }

    // Phase 2: delete all nodes (edges already removed).
    for node_id in &nodes_to_delete {
        if !detach && node::node_has_edges(conn, *node_id)? {
            return Err(GraphError::constraint(format!(
                "cannot delete node {} because it still has relationships. Use DETACH DELETE.",
                node_id
            ))
            .with_code(ErrorCode::DeleteConnectedNode));
        }
        // Clean up secondary indexes (btree + FTS) before removing the node
        // itself. Mirrors WriteTransaction::delete_node in transaction.rs.
        // Missing node (already deleted via duplicate in nodes_to_delete or
        // an earlier edge cascade) is silently ignored — index entries
        // would have been cleared at first removal.
        if let Ok(n) = node::get_node(conn, *node_id) {
            let _ = index::remove_indexes_for_node(conn, *node_id, &n.labels, &n.properties);
            let _ = fts::remove_fts_for_node(conn, *node_id, &n.labels, &n.properties);
        }
        let _ = node::delete_node(conn, *node_id);
    }

    Ok(records)
}

/// Collect entities from a variable binding for two-phase delete.
///
/// Node bindings are stored as `Value::I64(id)` plus an `alias.__id` marker;
/// a bare integer without the marker is an ordinary integer value (e.g. from
/// `WITH n.k AS x`) and must not be treated as a node reference.
pub(in crate::cypher::executor) fn collect_var_entities(
    rec: &NamedRecord,
    var: &str,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) -> Result<()> {
    let edge_src_key = format!("{var}.__src");
    let edge_dst_key = format!("{var}.__dst");
    let edge_type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
        rec.get(&edge_src_key),
        rec.get(&edge_dst_key),
        rec.get(&edge_type_key),
    ) {
        let edge_seq_key = format!("{var}.__edge_seq");
        let seq = if let Some(Value::I64(s)) = rec.get(&edge_seq_key) {
            Some(*s as u64)
        } else {
            None
        };
        edges.push((NodeId(*src as u64), NodeId(*dst as u64), label.clone(), seq));
        return Ok(());
    }
    match rec.get(var) {
        Some(Value::I64(id)) if is_node_binding(rec, var) => {
            nodes.push(NodeId(*id as u64));
            Ok(())
        }
        Some(val) => {
            let val = val.clone();
            collect_value_entities(&val, edges, nodes)
        }
        None => Ok(()),
    }
}

/// Whether `var` in `rec` is a node binding (flat `I64` id + `__id` marker).
pub(in crate::cypher::executor) fn is_node_binding(rec: &NamedRecord, var: &str) -> bool {
    matches!(rec.get(var), Some(Value::I64(_)))
        && matches!(rec.get(&format!("{var}.__id")), Some(Value::I64(_)))
}

fn not_an_entity(what: &str, val: &Value) -> GraphError {
    GraphError::type_error(
        crate::types::QueryPhase::Runtime,
        format!("{what} requires a node, relationship, or path, got {val:?}"),
    )
    .with_code(ErrorCode::InvalidArgumentType)
}

/// Collect entities from a Value for two-phase delete. Nulls are skipped;
/// any other non-entity value is a type error.
pub(in crate::cypher::executor) fn collect_value_entities(
    val: &Value,
    edges: &mut Vec<(NodeId, NodeId, String, Option<u64>)>,
    nodes: &mut Vec<NodeId>,
) -> Result<()> {
    match val {
        Value::Null => {}
        Value::Node(n) => {
            nodes.push(n.id);
        }
        Value::Edge(e) => {
            edges.push((e.src, e.dst, e.label.clone(), None));
        }
        Value::Path(p) => {
            for e in &p.edges {
                edges.push((e.src, e.dst, e.label.clone(), None));
            }
            for n in &p.nodes {
                nodes.push(n.id);
            }
        }
        Value::List(items) => {
            for item in items {
                collect_value_entities(item, edges, nodes)?;
            }
        }
        other => return Err(not_an_entity("DELETE", other)),
    }
    Ok(())
}

/// Reject `SET` / `REMOVE` on a binding that holds a plain (non-entity)
/// value. Entity bindings and nulls are handled by the callers.
fn check_set_target(rec: &NamedRecord, var: &str, what: &str) -> Result<()> {
    if rec.get(&format!("{var}.__src")).is_some() {
        return Ok(()); // relationship binding
    }
    match rec.get(var) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Node(_)) | Some(Value::Edge(_)) | Some(Value::Path(_)) => Ok(()),
        Some(Value::I64(_)) if is_node_binding(rec, var) => Ok(()),
        Some(other) => Err(not_an_entity(what, other)),
    }
}

pub(in crate::cypher::executor) fn exec_set_property(
    conn: &Connection,
    input: &LogicalOp,
    assignments: &[crate::cypher::ast::Assignment],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for assignment in assignments {
            let var = &assignment.variable;
            check_set_target(rec, var, "SET")?;
            // Check if this is a relationship variable (has edge identity metadata).
            let edge_src_key = format!("{var}.__src");
            let edge_dst_key = format!("{var}.__dst");
            let edge_type_key = format!("{var}.__type");
            if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
                rec.get(&edge_src_key),
                rec.get(&edge_dst_key),
                rec.get(&edge_type_key),
            ) {
                let val = eval_expr(
                    &assignment.value,
                    rec,
                    crate::cypher::eval::EvalCx::new(conn),
                )?;
                validate_write_value(&assignment.property, &val)?;
                let edge_seq_key = format!("{var}.__edge_seq");
                if let Some(Value::I64(seq)) = rec.get(&edge_seq_key) {
                    edge::set_edge_property_at(
                        conn,
                        NodeId(*src as u64),
                        NodeId(*dst as u64),
                        label,
                        *seq as u64,
                        &assignment.property,
                        val.clone(),
                    )?;
                } else {
                    edge::set_edge_property(
                        conn,
                        NodeId(*src as u64),
                        NodeId(*dst as u64),
                        label,
                        &assignment.property,
                        val.clone(),
                    )?;
                }
                // Update record so downstream RETURN sees the new value.
                let prop_key = format!("{var}.{}", assignment.property);
                if val == Value::Null {
                    rec.fields.swap_remove(&prop_key);
                } else {
                    rec.set(prop_key, val);
                }
            } else if let Some(Value::I64(id)) = rec.get(var) {
                let node_id = NodeId(*id as u64);
                let old = node::get_node(conn, node_id)?;
                let val = eval_expr(
                    &assignment.value,
                    rec,
                    crate::cypher::eval::EvalCx::new(conn),
                )?;
                validate_write_value(&assignment.property, &val)?;
                node::set_node_property(conn, node_id, &assignment.property, val.clone())?;
                let mut new_props = old.properties.clone();
                if val == Value::Null {
                    new_props.remove(&assignment.property);
                } else {
                    new_props.insert(assignment.property.clone(), val.clone());
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
                // Update record so downstream RETURN sees the new value.
                let prop_key = format!("{var}.{}", assignment.property);
                if val == Value::Null {
                    rec.fields.swap_remove(&prop_key);
                } else {
                    rec.set(prop_key, val);
                }
            }
        }
    }
    Ok(records)
}

thread_local! {
    /// `(max_property_value_bytes, max_name_bytes)` for the query currently
    /// executing on this thread; 0 = unlimited. Installed by
    /// [`WriteLimitsScope`] from the `ExecContext` in `execute_with_ctx`.
    static WRITE_LIMITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// RAII guard that installs the write limits for one query execution and
/// restores the previous limits (supporting nested executions) on drop.
pub(crate) struct WriteLimitsScope {
    prev: (usize, usize),
}

impl WriteLimitsScope {
    pub(crate) fn enter(max_property_value_bytes: usize, max_name_bytes: usize) -> Self {
        let prev = WRITE_LIMITS.with(|c| c.replace((max_property_value_bytes, max_name_bytes)));
        Self { prev }
    }
}

impl Drop for WriteLimitsScope {
    fn drop(&mut self) {
        WRITE_LIMITS.with(|c| c.set(self.prev));
    }
}

fn current_write_limits() -> (usize, usize) {
    let (v, n) = WRITE_LIMITS.with(|c| c.get());
    (
        if v == 0 { usize::MAX } else { v },
        if n == 0 { usize::MAX } else { n },
    )
}

/// Validate one property assignment written by Cypher: storable type
/// (no maps / entities, recursively in lists), key length, and value size.
pub(in crate::cypher::executor) fn validate_write_value(key: &str, val: &Value) -> Result<()> {
    validate_property_value(val)?;
    let (max_value, max_name) = current_write_limits();
    let one = Properties::from([(key.to_string(), val.clone())]);
    crate::types::validate_properties("", &one, max_name, max_value)
}

/// [`validate_write_value`] for every entry of a property map.
pub(in crate::cypher::executor) fn validate_write_properties(props: &Properties) -> Result<()> {
    for (k, v) in props {
        validate_write_value(k, v)?;
    }
    Ok(())
}

/// Validate label / relationship-type names written by Cypher against the
/// configured name-length limit.
pub(in crate::cypher::executor) fn validate_write_labels(labels: &[String]) -> Result<()> {
    let (_, max_name) = current_write_limits();
    for l in labels {
        crate::types::validate_name_length(l, max_name)?;
    }
    Ok(())
}

/// Validate that a value is storable as a property (no nested maps/nodes/edges).
pub(in crate::cypher::executor) fn validate_property_value(val: &Value) -> Result<()> {
    match val {
        Value::Map(_) | Value::Node(_) | Value::Edge(_) | Value::Path(_) => {
            return Err(GraphError::type_error(
                crate::types::QueryPhase::Runtime,
                "maps, nodes, relationships, and paths cannot be stored as properties".to_string(),
            )
            .with_code(ErrorCode::InvalidPropertyType));
        }
        Value::List(items) => {
            for item in items {
                validate_property_value(item)?;
            }
        }
        _ => {} // Scalars and Null are fine.
    }
    Ok(())
}

pub(in crate::cypher::executor) fn exec_set_label(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    labels: &[String],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        // Skip null variables (from OPTIONAL MATCH).
        check_set_target(rec, variable, "SET")?;
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            validate_write_labels(labels)?;
            for label in labels {
                node::add_node_label(conn, node_id, label)?;
            }
            // Adding labels can bring new per-label indexes into scope; insert
            // entries across the full (post-add) label set. Idempotent for
            // labels the node already had.
            let new = node::get_node(conn, node_id)?;
            index::update_indexes_for_node(conn, node_id, &new.labels, None, &new.properties)?;
            fts::update_fts_for_node(conn, node_id, &new.labels, None, &new.properties)?;
            // Update the labels in the record (__labels list and __label colon-joined string).
            let labels_key = format!("{variable}.__labels");
            if let Some(Value::List(current_labels)) = rec.get(&labels_key) {
                let mut updated = current_labels.clone();
                for label in labels {
                    let val = Value::String(label.clone());
                    if !updated.contains(&val) {
                        updated.push(val);
                    }
                }
                // Sort for consistency.
                updated.sort_by(|a, b| {
                    let sa = if let Value::String(s) = a {
                        s.as_str()
                    } else {
                        ""
                    };
                    let sb = if let Value::String(s) = b {
                        s.as_str()
                    } else {
                        ""
                    };
                    sa.cmp(sb)
                });
                // Update the colon-joined __label string.
                let label_strs: Vec<&str> = updated
                    .iter()
                    .filter_map(|v| {
                        if let Value::String(s) = v {
                            Some(s.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                let label_key = format!("{variable}.__label");
                rec.set(label_key, Value::String(label_strs.join(":")));
                rec.set(labels_key, Value::List(updated));
            }
        }
    }
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_set_properties(
    conn: &Connection,
    input: &LogicalOp,
    variable: &str,
    value_expr: &crate::cypher::ast::Expr,
    merge: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        // Edge variant: the variable carries edge identity metadata
        // (`__src`/`__dst`/`__type`, plus `__edge_seq` for parallel edges).
        let edge_src_key = format!("{variable}.__src");
        let edge_dst_key = format!("{variable}.__dst");
        let edge_type_key = format!("{variable}.__type");
        if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) = (
            rec.get(&edge_src_key),
            rec.get(&edge_dst_key),
            rec.get(&edge_type_key),
        ) {
            let src = NodeId(*src as u64);
            let dst = NodeId(*dst as u64);
            let label = label.clone();
            let edge_seq_key = format!("{variable}.__edge_seq");
            let seq = match rec.get(&edge_seq_key) {
                Some(Value::I64(s)) => *s as u64,
                _ => {
                    let prefix = crate::edge::edge_props_prefix(src, dst, &label);
                    let entries = crate::storage::kv::scan_prefix(
                        conn,
                        crate::storage::kv::TABLE_EDGE_PROPS,
                        &prefix,
                    )?;
                    entries
                        .first()
                        .map(|(k, _)| crate::edge::edge_seq_from_key(k, prefix.len()))
                        .unwrap_or(0)
                }
            };

            let map_val = eval_expr(value_expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
            let map = match &map_val {
                Value::Map(m) => m,
                Value::Null => continue,
                _ => {
                    return Err(GraphError::semantic("SET properties requires a map value"));
                }
            };

            let old_props = edge::get_edge_properties_at(conn, src, dst, &label, seq)?;
            let new_props: Properties = if merge {
                let mut props = old_props.clone();
                for (k, v) in map {
                    if *v == Value::Null {
                        props.remove(k);
                    } else {
                        validate_write_value(k, v)?;
                        crate::types::validate_name(k)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            } else {
                let mut props = Properties::new();
                for (k, v) in map {
                    if *v != Value::Null {
                        validate_write_value(k, v)?;
                        crate::types::validate_name(k)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            };

            edge::set_all_edge_properties_at(conn, src, dst, &label, seq, new_props.clone())?;

            // Update the record so downstream RETURN sees the new values.
            for key in old_props.keys() {
                let prop_key = format!("{variable}.{key}");
                rec.remove(&prop_key);
            }
            for (key, val) in &new_props {
                let prop_key = format!("{variable}.{key}");
                rec.set(prop_key, val.clone());
            }
            continue;
        }

        // Skip null variables (from OPTIONAL MATCH).
        check_set_target(rec, variable, "SET")?;
        if let Some(Value::I64(id)) = rec.get(variable) {
            let node_id = NodeId(*id as u64);
            let map_val = eval_expr(value_expr, rec, crate::cypher::eval::EvalCx::new(conn))?;

            // The map value must be a Map (or Null to skip).
            let map = match &map_val {
                Value::Map(m) => m,
                Value::Null => continue,
                _ => {
                    return Err(GraphError::semantic("SET properties requires a map value"));
                }
            };

            let old = node::get_node(conn, node_id)?;
            let old_props = old.properties.clone();

            let new_props: Properties = if merge {
                // Merge: start from existing, overlay map, remove nulls.
                let mut props = old.properties.clone();
                for (k, v) in map {
                    if *v == Value::Null {
                        props.remove(k);
                    } else {
                        validate_write_value(k, v)?;
                        crate::types::validate_name(k)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            } else {
                // Overwrite: start from empty, add non-null entries from map.
                let mut props = Properties::new();
                for (k, v) in map {
                    if *v != Value::Null {
                        validate_write_value(k, v)?;
                        crate::types::validate_name(k)?;
                        props.insert(k.clone(), v.clone());
                    }
                }
                props
            };

            node::set_all_node_properties(conn, node_id, new_props.clone())?;
            index::update_indexes_for_node(
                conn,
                node_id,
                &old.labels,
                Some(&old_props),
                &new_props,
            )?;
            fts::update_fts_for_node(conn, node_id, &old.labels, Some(&old_props), &new_props)?;

            // Update the record: remove old property keys, add new ones.
            // First, remove all old flattened property keys.
            for key in old_props.keys() {
                let prop_key = format!("{variable}.{key}");
                rec.remove(&prop_key);
            }
            // Add new property keys.
            for (key, val) in &new_props {
                let prop_key = format!("{variable}.{key}");
                rec.set(prop_key, val.clone());
            }
        }
    }
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_remove(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::RemoveItem],
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let mut records = exec(conn, input, ctx)?;
    for rec in &mut records {
        for item in items {
            match item {
                crate::cypher::ast::RemoveItem::Property { variable, property } => {
                    check_set_target(rec, variable, "REMOVE")?;
                    // Check if this is an edge variable.
                    let edge_src_key = format!("{variable}.__src");
                    let edge_dst_key = format!("{variable}.__dst");
                    let edge_type_key = format!("{variable}.__type");
                    if let (
                        Some(Value::I64(src)),
                        Some(Value::I64(dst)),
                        Some(Value::String(label)),
                    ) = (
                        rec.get(&edge_src_key),
                        rec.get(&edge_dst_key),
                        rec.get(&edge_type_key),
                    ) {
                        let src = *src;
                        let dst = *dst;
                        let label = label.clone();
                        // Remove edge property by setting to Null.
                        let edge_seq_key = format!("{variable}.__edge_seq");
                        if let Some(Value::I64(seq)) = rec.get(&edge_seq_key) {
                            let seq = *seq;
                            edge::set_edge_property_at(
                                conn,
                                NodeId(src as u64),
                                NodeId(dst as u64),
                                &label,
                                seq as u64,
                                property,
                                Value::Null,
                            )?;
                        } else {
                            edge::set_edge_property(
                                conn,
                                NodeId(src as u64),
                                NodeId(dst as u64),
                                &label,
                                property,
                                Value::Null,
                            )?;
                        }
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.fields.swap_remove(&prop_key);
                    } else if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        let old = node::get_node(conn, node_id)?;
                        node::remove_node_property(conn, node_id, property)?;
                        let mut new_props = old.properties.clone();
                        new_props.remove(property);
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
                        // Update record to reflect removal.
                        let prop_key = format!("{variable}.{property}");
                        rec.fields.swap_remove(&prop_key);
                    }
                }
                crate::cypher::ast::RemoveItem::Label { variable, labels } => {
                    check_set_target(rec, variable, "REMOVE")?;
                    if let Some(Value::I64(id)) = rec.get(variable) {
                        let id = *id;
                        let node_id = NodeId(id as u64);
                        // A removed label's per-label index entries must be
                        // dropped; entries owned by the node's remaining labels
                        // stay valid.
                        let old = node::get_node(conn, node_id)?;
                        for label in labels {
                            index::remove_indexes_for_label(conn, node_id, label, &old.properties)?;
                            fts::remove_fts_for_label(conn, node_id, label, &old.properties)?;
                            node::remove_node_label(conn, node_id, label)?;
                        }
                        // Update the labels in the record.
                        let label_key = format!("{variable}.__labels");
                        if let Some(Value::List(current_labels)) = rec.get(&label_key) {
                            let updated: Vec<Value> = current_labels
                                .iter()
                                .filter(|l| {
                                    if let Value::String(s) = l {
                                        !labels.contains(s)
                                    } else {
                                        true
                                    }
                                })
                                .cloned()
                                .collect();
                            rec.set(label_key, Value::List(updated));
                        }
                    }
                }
            }
        }
    }
    Ok(records)
}

/// Execute `CREATE INDEX ON :Label(prop1, prop2, ...)`.
///
/// Delegates to `index::create_composite_index` (which handles both single-
/// property and composite cases via the same table schema).
pub(in crate::cypher::executor) fn exec_create_index(
    conn: &Connection,
    label: &str,
    properties: &[String],
) -> Result<Vec<NamedRecord>> {
    let props: Vec<&str> = properties.iter().map(String::as_str).collect();
    index::create_composite_index(conn, label, &props)?;
    Ok(vec![])
}

/// Execute `DROP INDEX ON :Label(prop1, prop2, ...)`.
pub(in crate::cypher::executor) fn exec_drop_index(
    conn: &Connection,
    label: &str,
    properties: &[String],
) -> Result<Vec<NamedRecord>> {
    let props: Vec<&str> = properties.iter().map(String::as_str).collect();
    index::drop_composite_index(conn, label, &props)?;
    Ok(vec![])
}
