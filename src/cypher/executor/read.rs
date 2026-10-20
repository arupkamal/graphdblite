//! Read operators — Scan, IndexLookup, Expand, Filter, Project, CrossProduct, Unwind, plus compound-binding helpers.

use rusqlite::Connection;

use crate::cypher::ast::*;
use crate::cypher::eval::{eval_expr, eval_predicate, expr_to_column_name};
use crate::cypher::ir::LookupKey;
use crate::cypher::record::NamedRecord;
use crate::cypher::record_view::RecordView;
use crate::types::*;
use crate::{edge, index, node};

use super::util::*;
use super::*;

pub(in crate::cypher::executor) fn exec_scan(
    conn: &Connection,
    label: &str,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::with_capacity(nodes.len());
    for n in nodes {
        records.push(node_to_record(&n, alias));
    }
    check_row_limit(&records, ctx)?;
    Ok(records)
}

pub(in crate::cypher::executor) fn exec_id_lookup(
    conn: &Connection,
    alias: &str,
    value_expr: &Expr,
    record: &NamedRecord,
) -> Result<Vec<NamedRecord>> {
    let evaluated = eval_expr(value_expr, record, crate::cypher::eval::EvalCx::new(conn))?;
    let id = match evaluated {
        Value::I64(n) if n >= 0 => NodeId(n as u64),
        // Anything else (negative, null, non-integer, error) yields zero
        // rows rather than an error — matches the semantics of an
        // unmatched WHERE predicate.
        _ => return Ok(Vec::new()),
    };
    match node::get_node(conn, id) {
        Ok(n) => Ok(vec![node_to_record(&n, alias)]),
        Err(GraphError::NodeNotFound { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_fulltext_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term_expr: &Expr,
    remaining_filters: Option<&Expr>,
    record: &NamedRecord,
) -> Result<Vec<NamedRecord>> {
    let evaluated = eval_expr(term_expr, record, crate::cypher::eval::EvalCx::new(conn))?;
    let term = match evaluated {
        Value::String(s) => s,
        Value::Null => return Ok(Vec::new()),
        // `toLower(<non-string>)` is null, so the predicate can never hold.
        _ if case_insensitive => return Ok(Vec::new()),
        other => {
            return Err(GraphError::type_error(
                QueryPhase::Runtime,
                format!(
                    "fulltext predicate on '{}' requires a String term, got {}",
                    property,
                    fts_value_type_name(&other),
                ),
            ));
        }
    };

    // The FTS index only narrows candidates; `fts_predicate_matches` decides.
    // A case-insensitive lookup with non-ASCII text could disagree with Rust's
    // `to_lowercase` (FTS5 uses its own Unicode folding), so scan instead.
    let candidates = if case_insensitive && !term.is_ascii() {
        None
    } else {
        crate::fts::fulltext_lookup(conn, label, property, &term)?
    };
    let scored_ids = match candidates {
        Some(rows) => rows,
        None => {
            // Term is below the trigram floor (<3 codepoints). Fall back to
            // a full label scan with per-row predicate evaluation.
            return exec_fulltext_fallback(
                conn,
                label,
                alias,
                property,
                op,
                case_insensitive,
                &term,
                remaining_filters,
            );
        }
    };

    let mut records = Vec::new();
    for (id, rank) in scored_ids {
        let n = node::get_node(conn, id)?;
        // Re-check every candidate with the exact predicate: anchored ops need
        // a position check, and a case-insensitive index returns candidates
        // that a case-sensitive predicate must reject (and vice versa).
        let Some(Value::String(s)) = n.properties.get(property) else {
            continue;
        };
        if !fts_predicate_matches(s, op, case_insensitive, &term) {
            continue;
        }
        let mut rec = node_to_record(&n, alias);
        // Surface BM25 score for `score(<alias>)` projections. FTS5's
        // `bm25()` is negative-signed (lower = better); negate so the
        // user-facing convention is higher = better, matching the
        // `fts.search` procedure path.
        rec.set(format!("{alias}.__fts_score"), Value::F64(-rank));
        if let Some(filter) = remaining_filters {
            if !eval_predicate(filter, &rec, crate::cypher::eval::EvalCx::new(conn))? {
                continue;
            }
        }
        records.push(rec);
    }
    Ok(records)
}

/// Evaluate the string predicate a `FullTextLookup` stands for:
/// `s <op> term` (case-sensitive), or `toLower(s) <op> toLower(term)`.
fn fts_predicate_matches(
    s: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term: &str,
) -> bool {
    use crate::cypher::ir::FullTextOp;
    let (s, term): (std::borrow::Cow<str>, std::borrow::Cow<str>) = if case_insensitive {
        (s.to_lowercase().into(), term.to_lowercase().into())
    } else {
        (s.into(), term.into())
    };
    match op {
        FullTextOp::Contains => s.contains(term.as_ref()),
        FullTextOp::StartsWith => s.starts_with(term.as_ref()),
        FullTextOp::EndsWith => s.ends_with(term.as_ref()),
    }
}

/// Fallback for when the term is below the trigram floor (<3 codepoints).
/// Performs a full label scan with per-row string predicate evaluation.
#[allow(clippy::too_many_arguments)]
fn exec_fulltext_fallback(
    conn: &Connection,
    label: &str,
    alias: &str,
    property: &str,
    op: crate::cypher::ir::FullTextOp,
    case_insensitive: bool,
    term: &str,
    remaining_filters: Option<&Expr>,
) -> Result<Vec<NamedRecord>> {
    let nodes = node::find_nodes_by_label(conn, label)?;
    let mut records = Vec::new();
    for n in nodes {
        let Some(Value::String(s)) = n.properties.get(property) else {
            continue;
        };
        if !fts_predicate_matches(s, op, case_insensitive, term) {
            continue;
        }
        let mut rec = node_to_record(&n, alias);
        // Trigram-floor fallback rows still matched via an FTS-eligible op,
        // so emit a concrete 0.0 score rather than leaving `score(n)` NULL.
        rec.set(format!("{alias}.__fts_score"), Value::F64(0.0));
        if let Some(filter) = remaining_filters {
            if !eval_predicate(filter, &rec, crate::cypher::eval::EvalCx::new(conn))? {
                continue;
            }
        }
        records.push(rec);
    }
    Ok(records)
}

/// Returns a human-readable type name for a `Value` — used in FTS type-error messages.
fn fts_value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::I64(_) => "Integer",
        Value::F64(_) => "Float",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Map(_) => "Map",
        Value::Node(_) => "Node",
        Value::Edge(_) => "Relationship",
        Value::Path(_) => "Path",
        Value::Date(_) => "Date",
        Value::Time(_) => "Time",
        Value::LocalTime(_) => "LocalTime",
        Value::DateTime(_) => "DateTime",
        Value::LocalDateTime(_) => "LocalDateTime",
        Value::Duration(_) => "Duration",
    }
}

pub(in crate::cypher::executor) fn exec_index_lookup(
    conn: &Connection,
    label: &str,
    alias: &str,
    index_properties: &[String],
    lookups: &[(String, LookupKey)],
    remaining_filters: Option<&Expr>,
) -> Result<Vec<NamedRecord>> {
    let node_ids = index_lookup_ids(conn, label, index_properties, lookups)?;
    let mut records = Vec::new();

    for id in node_ids {
        let n = node::get_node(conn, id)?;
        // Defense-in-depth: only emit nodes that actually carry the queried
        // label. Index table names historically could collide across
        // (label, property) pairs containing underscores, so a lookup could
        // surface a node stored under a different label; re-checking membership
        // here guarantees index-served results never leak a cross-label node.
        if !label.is_empty() && !n.labels.iter().any(|l| l == label) {
            continue;
        }
        let rec = node_to_record(&n, alias);

        if let Some(filter) = remaining_filters {
            if !eval_predicate(filter, &rec, crate::cypher::eval::EvalCx::new(conn))? {
                continue;
            }
        }

        records.push(rec);
    }

    Ok(records)
}

/// Evaluate `lookups` to concrete values and call `composite_index_prefix_lookup`.
/// `index_properties` is the full property list of the chosen index (from the IR),
/// while `lookups` is the matched prefix (len <= index_properties.len()).
/// Works for N=1 (single-prop) and N>=2 (composite prefix) uniformly.
pub(crate) fn index_lookup_ids(
    conn: &Connection,
    label: &str,
    index_properties: &[String],
    lookups: &[(String, LookupKey)],
) -> Result<Vec<NodeId>> {
    let values: Vec<Value> = lookups
        .iter()
        .map(|(_, k)| crate::cypher::executor::resolve_lookup_key(k))
        .collect::<Result<_>>()?;
    let full_props_refs: Vec<&str> = index_properties.iter().map(String::as_str).collect();

    // Cypher equality semantics, which the index (keyed on exact msgpack bytes)
    // does not provide on its own:
    // - `x = null` (and anything containing null / NaN) is never true;
    // - integers and floats compare numerically (`1 = 1.0`, `0.0 = -0.0`).
    // Expand each value into every stored representation it is equal to and
    // union the lookups. Pathologically large expansions fall back to a label
    // scan with an explicit equality check.
    let mut per_value: Vec<Vec<Value>> = Vec::with_capacity(values.len());
    let mut combos: usize = 1;
    for v in &values {
        match index_equality_variants(v) {
            EqVariants::Never => return Ok(Vec::new()),
            EqVariants::TooMany => combos = usize::MAX,
            EqVariants::Values(vs) => {
                combos = combos.saturating_mul(vs.len());
                per_value.push(vs);
            }
        }
    }
    if combos > MAX_INDEX_LOOKUP_VARIANTS {
        let props: Vec<&str> = lookups.iter().map(|(p, _)| p.as_str()).collect();
        return Ok(node::find_nodes_by_label(conn, label)?
            .into_iter()
            .filter(|n| {
                props.iter().zip(&values).all(|(p, v)| {
                    n.properties
                        .get(*p)
                        .is_some_and(|stored| cypher_values_equal(stored, v))
                })
            })
            .map(|n| n.id)
            .collect());
    }
    if combos == 1 {
        let single: Vec<Value> = per_value.into_iter().map(|mut v| v.remove(0)).collect();
        return index::composite_index_prefix_lookup(conn, label, &full_props_refs, &single);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut combo: Vec<Value> = Vec::with_capacity(per_value.len());
    fn walk(
        conn: &Connection,
        label: &str,
        props: &[&str],
        per_value: &[Vec<Value>],
        combo: &mut Vec<Value>,
        seen: &mut std::collections::HashSet<NodeId>,
        out: &mut Vec<NodeId>,
    ) -> Result<()> {
        if combo.len() == per_value.len() {
            for id in index::composite_index_prefix_lookup(conn, label, props, combo)? {
                if seen.insert(id) {
                    out.push(id);
                }
            }
            return Ok(());
        }
        for v in &per_value[combo.len()] {
            combo.push(v.clone());
            walk(conn, label, props, per_value, combo, seen, out)?;
            combo.pop();
        }
        Ok(())
    }
    walk(
        conn,
        label,
        &full_props_refs,
        &per_value,
        &mut combo,
        &mut seen,
        &mut out,
    )?;
    Ok(out)
}

/// Upper bound on the number of exact-representation lookups a single index
/// probe may expand into (see [`index_equality_variants`]).
const MAX_INDEX_LOOKUP_VARIANTS: usize = 64;

/// Result of [`index_equality_variants`].
enum EqVariants {
    /// Nothing is Cypher-equal to the value (null, NaN, or a list containing one).
    Never,
    /// Every exact stored representation that is Cypher-equal to the value.
    Values(Vec<Value>),
    /// Too many representations to enumerate; the caller should scan.
    TooMany,
}

/// Every stored value that is Cypher-equal to `v` (e.g. `1` is equal to the
/// stored `1` and `1.0`; a list expands element-wise).
fn index_equality_variants(v: &Value) -> EqVariants {
    const EXACT_F64_INT_LIMIT: i64 = 1 << 53;
    match v {
        Value::Null => EqVariants::Never,
        Value::F64(f) if f.is_nan() => EqVariants::Never,
        Value::I64(i) => {
            let mut out = vec![Value::I64(*i)];
            if i.unsigned_abs() <= EXACT_F64_INT_LIMIT as u64 {
                out.push(Value::F64(*i as f64));
                if *i == 0 {
                    out.push(Value::F64(-0.0));
                }
            }
            EqVariants::Values(out)
        }
        Value::F64(f) => {
            let mut out = vec![Value::F64(*f)];
            if *f == 0.0 {
                out.push(Value::F64(if f.is_sign_negative() { 0.0 } else { -0.0 }));
            }
            if f.fract() == 0.0 && f.abs() <= EXACT_F64_INT_LIMIT as f64 {
                out.push(Value::I64(*f as i64));
            }
            EqVariants::Values(out)
        }
        Value::List(items) => {
            let mut acc: Vec<Vec<Value>> = vec![Vec::with_capacity(items.len())];
            for item in items {
                let vs = match index_equality_variants(item) {
                    EqVariants::Values(vs) => vs,
                    other => return other,
                };
                if acc.len().saturating_mul(vs.len()) > MAX_INDEX_LOOKUP_VARIANTS {
                    return EqVariants::TooMany;
                }
                let mut next = Vec::with_capacity(acc.len() * vs.len());
                for prefix in &acc {
                    for alt in &vs {
                        let mut p = prefix.clone();
                        p.push(alt.clone());
                        next.push(p);
                    }
                }
                acc = next;
            }
            EqVariants::Values(acc.into_iter().map(Value::List).collect())
        }
        other => EqVariants::Values(vec![other.clone()]),
    }
}

/// Cypher `=` for stored property values: numeric cross-type comparison,
/// element-wise for lists, and plain equality otherwise. Null never matches.
fn cypher_values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::I64(x), Value::F64(y)) | (Value::F64(y), Value::I64(x)) => (*x as f64) == *y,
        (Value::F64(x), Value::F64(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| cypher_values_equal(p, q))
        }
        _ => a == b,
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::cypher::executor) fn exec_expand(
    conn: &Connection,
    input: &LogicalOp,
    src_alias: &str,
    dst_alias: &str,
    rel_alias: Option<&str>,
    edge_types: &[String],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    var_length: bool,
    var_length_prop_filters: &HashMap<String, Expr>,
    result_cap: Option<usize>,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let input_records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &input_records {
        // Relationship property constraints on a var-length pattern may be
        // any expression (parameters, variables, arithmetic) — evaluate them
        // against the current row rather than keeping only literals.
        let prop_filter_values = eval_var_length_prop_filters(conn, var_length_prop_filters, rec)?;
        if let Some(cap) = result_cap {
            if results.len() >= cap {
                break;
            }
        }
        let per_call_cap = result_cap.map(|c| c.saturating_sub(results.len()));
        let expanded = expand_record(
            conn,
            rec,
            src_alias,
            dst_alias,
            rel_alias,
            edge_types,
            direction,
            min_hops,
            max_hops,
            var_length,
            &prop_filter_values,
            per_call_cap,
            ctx.max_traversal_work,
        )?;
        results.extend(expanded);
    }

    check_row_limit(&results, ctx)?;
    Ok(results)
}

/// Evaluate the relationship property map of a var-length pattern against
/// `rec`. Every entry is kept: dropping non-literal entries would silently
/// widen the match.
pub(crate) fn eval_var_length_prop_filters(
    conn: &Connection,
    filters: &HashMap<String, Expr>,
    rec: &NamedRecord,
) -> Result<HashMap<String, Value>> {
    let mut out = HashMap::with_capacity(filters.len());
    for (k, expr) in filters {
        let v = match &expr.kind {
            ExprKind::Literal(lit) => literal_to_value(lit),
            _ => eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?,
        };
        out.insert(k.clone(), v);
    }
    Ok(out)
}

/// Expand one input record into zero or more output records. Handles every
/// shape `exec_expand` does — labeled or untyped, single-hop or var-length,
/// any [`Direction`] including [`Direction::Both`] with parallel edges —
/// so callers (the materialized `exec_expand`, the slot-path `ExpandIter`)
/// share the same traversal semantics.
#[allow(clippy::too_many_arguments)]
pub(crate) fn expand_record(
    conn: &Connection,
    rec: &NamedRecord,
    src_alias: &str,
    dst_alias: &str,
    rel_alias: Option<&str>,
    edge_types: &[String],
    direction: Direction,
    min_hops: u32,
    max_hops: u32,
    var_length: bool,
    var_length_prop_filters: &HashMap<String, Value>,
    result_cap: Option<usize>,
    max_traversal_work: u64,
) -> Result<Vec<NamedRecord>> {
    let mut results = Vec::new();
    let src_id = match rec.get(src_alias).and_then(value_to_node_id) {
        Some(id) => id,
        _ => return Ok(results),
    };

    // If no types specified, discover all edge types for this node.
    // For var-length, pass empty labels so traverse_paths discovers
    // types at each hop (different nodes may have different edge types).
    let _owned_labels: Vec<String>;
    let labels: Vec<&str> = if edge_types.is_empty() {
        let all = edge::get_all_edge_labels(conn, src_id, direction)?;
        _owned_labels = all.into_iter().map(|(l, _)| l).collect();
        _owned_labels.iter().map(|s| s.as_str()).collect()
    } else {
        edge_types.iter().map(|s| s.as_str()).collect()
    };
    let var_length_labels: Vec<&str> = if edge_types.is_empty() {
        vec![] // signal traverse_paths to discover per-hop
    } else {
        labels.clone()
    };

    // If the destination alias is already bound in the record (cyclic
    // pattern like `(a)-[:R]->(b)-[:S]->(a)`), we must only keep
    // expansions where the destination equals the bound node.
    let bound_dst_id = rec.get(dst_alias).and_then(value_to_node_id);

    if var_length {
        let paths = edge::traverse_paths(
            conn,
            src_id,
            &var_length_labels,
            direction,
            min_hops,
            max_hops,
            var_length_prop_filters,
            result_cap,
            max_traversal_work,
        )?;
        for (dst_id, steps) in paths {
            if let Some(required) = bound_dst_id {
                if dst_id != required {
                    continue;
                }
            }
            let mut new_rec = rec.clone();
            let _dst_node = fetch_and_populate(conn, &mut new_rec, dst_id, dst_alias)?;
            if let Some(r_alias) = rel_alias {
                let edge_list: Vec<Value> = steps
                    .iter()
                    .map(|step| {
                        let props = edge::get_edge_properties_at(
                            conn,
                            step.edge_src,
                            step.edge_dst,
                            &step.edge_label,
                            step.edge_seq,
                        )
                        .unwrap_or_default();
                        Value::Edge(crate::types::Edge {
                            src: step.edge_src,
                            dst: step.edge_dst,
                            label: step.edge_label.clone(),
                            properties: props,
                        })
                    })
                    .collect();
                new_rec.set(r_alias.to_string(), Value::List(edge_list));
            }
            results.push(new_rec);
        }
    } else {
        for &label in &labels {
            let neighbors = edge::get_neighbors(conn, src_id, label, direction)?;
            for dst_id in neighbors {
                if let Some(required) = bound_dst_id {
                    if dst_id != required {
                        continue;
                    }
                }

                // Fast path when the pattern doesn't bind the relationship:
                // emit one record per neighbor without scanning edge props
                // or computing edge direction. Mirrors what
                // `iter::ExpandIter`'s pre-`expand_record` path used to do.
                if rel_alias.is_none() {
                    let dst_node = node::get_node(conn, dst_id)?;
                    let mut new_rec = rec.clone();
                    populate_node_bindings(&mut new_rec, &dst_node, dst_alias);
                    results.push(new_rec);
                    continue;
                }

                let (edge_src, edge_dst) = match direction {
                    Direction::Incoming => (dst_id, src_id),
                    Direction::Outgoing => (src_id, dst_id),
                    Direction::Both => {
                        if edge::edge_exists(conn, src_id, dst_id, label).unwrap_or(false) {
                            (src_id, dst_id)
                        } else {
                            (dst_id, src_id)
                        }
                    }
                };

                let all_edges = edge::get_all_edge_props(conn, edge_src, edge_dst, label)?;
                let edge_list: Vec<(u64, Properties)> = if all_edges.is_empty() {
                    vec![(0, Properties::new())]
                } else {
                    all_edges
                };

                let dst_node = node::get_node(conn, dst_id)?;

                for (edge_seq, edge_props) in &edge_list {
                    let mut new_rec = rec.clone();
                    populate_node_bindings(&mut new_rec, &dst_node, dst_alias);

                    if let Some(r_alias) = rel_alias {
                        let (es, ed) = (edge_src.0 as i64, edge_dst.0 as i64);
                        let edge_key = (es.min(ed), es.max(ed), label, *edge_seq);
                        let mut duplicate = false;
                        for (key, _val) in &new_rec.fields {
                            if key.ends_with(".__src") && key != &format!("{r_alias}.__src") {
                                let other_alias = &key[..key.len() - 6];
                                let other_seq =
                                    match new_rec.get(&format!("{other_alias}.__edge_seq")) {
                                        Some(Value::I64(s)) => *s as u64,
                                        _ => 0,
                                    };
                                if let (
                                    Some(Value::I64(os)),
                                    Some(Value::I64(od)),
                                    Some(Value::String(ot)),
                                ) = (
                                    new_rec.get(key),
                                    new_rec.get(&format!("{other_alias}.__dst")),
                                    new_rec.get(&format!("{other_alias}.__type")),
                                ) {
                                    let other_key =
                                        ((*os).min(*od), (*os).max(*od), ot.as_str(), other_seq);
                                    if other_key == edge_key {
                                        duplicate = true;
                                        break;
                                    }
                                }
                            }
                        }
                        if duplicate {
                            continue;
                        }

                        new_rec.set(r_alias.to_string(), Value::String(label.to_string()));
                        new_rec.set(format!("{r_alias}.__src"), Value::I64(edge_src.0 as i64));
                        new_rec.set(format!("{r_alias}.__dst"), Value::I64(edge_dst.0 as i64));
                        new_rec.set(
                            format!("{r_alias}.__type"),
                            Value::String(label.to_string()),
                        );
                        new_rec.set(
                            format!("{r_alias}.__edge_seq"),
                            Value::I64(*edge_seq as i64),
                        );
                        for (key, val) in edge_props {
                            new_rec.set(format!("{r_alias}.{key}"), val.clone());
                        }
                    }
                    results.push(new_rec);
                }
            }
        }
    }

    Ok(results)
}

pub(in crate::cypher::executor) fn exec_cross_product(
    conn: &Connection,
    left: &LogicalOp,
    right: &LogicalOp,
    same_match: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    // Materialize only the left side. Re-execute the right side per left
    // record so peak memory is O(left + right + output) instead of
    // O(left * right).
    let left_records = exec(conn, left, ctx)?;
    let mut results = Vec::new();
    for l in &left_records {
        let right_records = exec(conn, right, ctx)?;
        for r in &right_records {
            let mut combined = l.clone();
            for (key, val) in &r.fields {
                combined.set(key.clone(), val.clone());
            }
            // Cross-pattern relationship uniqueness (only within the same
            // MATCH clause — separate MATCHes have independent scopes).
            if same_match && has_duplicate_relationships(&combined) {
                continue;
            }
            results.push(combined);
            if ctx.max_result_rows > 0 && results.len() > ctx.max_result_rows {
                return Err(GraphError::constraint(format!(
                    "cross product exceeded maximum of {} rows",
                    ctx.max_result_rows
                )));
            }
        }
    }
    Ok(results)
}

/// Collect edge identities from flat relationship bindings in a record
/// (`alias.__src`, `alias.__dst`, `alias.__type`). Returns a vec of
/// `(min(src,dst), max(src,dst), type, seq)` tuples.
pub(in crate::cypher::executor) fn collect_flat_edge_ids(
    rec: &NamedRecord,
) -> Vec<(i64, i64, String, u64)> {
    let mut edges = Vec::new();
    for (key, _) in &rec.fields {
        if key.ends_with(".__src") {
            let alias = &key[..key.len() - 6];
            let seq = match rec.get(&format!("{alias}.__edge_seq")) {
                Some(Value::I64(s)) => *s as u64,
                _ => 0,
            };
            if let (Some(Value::I64(s)), Some(Value::I64(d)), Some(Value::String(t))) = (
                rec.get(key),
                rec.get(&format!("{alias}.__dst")),
                rec.get(&format!("{alias}.__type")),
            ) {
                edges.push(((*s).min(*d), (*s).max(*d), t.clone(), seq));
            }
        }
    }
    edges
}

/// Check if a record contains two relationship bindings that refer to the
/// same underlying edge (same normalized src/dst/type/seq).
pub(in crate::cypher::executor) fn has_duplicate_relationships(rec: &NamedRecord) -> bool {
    let mut seen: Vec<(i64, i64, String, u64)> = Vec::new();
    for (key, _) in &rec.fields {
        if key.ends_with(".__src") {
            let alias = &key[..key.len() - 6];
            let seq = match rec.get(&format!("{alias}.__edge_seq")) {
                Some(Value::I64(s)) => *s as u64,
                _ => 0,
            };
            if let (Some(Value::I64(s)), Some(Value::I64(d)), Some(Value::String(t))) = (
                rec.get(key),
                rec.get(&format!("{alias}.__dst")),
                rec.get(&format!("{alias}.__type")),
            ) {
                let edge_key = ((*s).min(*d), (*s).max(*d), t.clone(), seq);
                if seen.contains(&edge_key) {
                    return true;
                }
                seen.push(edge_key);
            }
        }
    }
    false
}

pub(in crate::cypher::executor) fn exec_filter(
    conn: &Connection,
    input: &LogicalOp,
    predicate: &Expr,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();
    for rec in records {
        if eval_predicate(predicate, &rec, crate::cypher::eval::EvalCx::new(conn))? {
            results.push(rec);
        }
    }
    Ok(results)
}

/// Reconstruct a compound `Value::Node` or `Value::Edge` from a variable's
/// flat bindings in a record. Returns `None` if the variable has no node/edge
/// metadata (i.e. it's an expression/aggregate result, not a pattern binding).
///
/// Records flow through the pipeline with a flattened shape (`n.name`,
/// `n.__id`, etc.); this helper materializes the compound at projection time
/// so query results look the way openCypher specifies.
pub(crate) fn build_compound_binding(rec: &dyn RecordView, var: &str) -> Option<Value> {
    use crate::types::{Edge, Node, Properties};

    // Var-length relationship variables stored directly as Value::List.
    if let Some(val @ Value::List(_)) = rec.get(var) {
        return Some(val.clone());
    }
    // Path values stored directly.
    if let Some(val @ Value::Path(_)) = rec.get(var) {
        return Some(val.clone());
    }

    let prefix = format!("{var}.");

    // Edge binding: has __src / __dst / __type metadata.
    let src_key = format!("{var}.__src");
    let dst_key = format!("{var}.__dst");
    let type_key = format!("{var}.__type");
    if let (Some(Value::I64(src)), Some(Value::I64(dst)), Some(Value::String(label))) =
        (rec.get(&src_key), rec.get(&dst_key), rec.get(&type_key))
    {
        let src = *src;
        let dst = *dst;
        let label = label.clone();
        let mut properties = Properties::new();
        rec.for_each_field(&mut |key, val| {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        });
        return Some(Value::Edge(Edge {
            src: NodeId(src as u64),
            dst: NodeId(dst as u64),
            label,
            properties,
        }));
    }

    // Node binding: has __id / __label metadata.
    let id_key = format!("{var}.__id");
    let label_key = format!("{var}.__label");
    if let (Some(Value::I64(id)), Some(Value::String(label_str))) =
        (rec.get(&id_key), rec.get(&label_key))
    {
        let id = *id;
        let label_str = label_str.clone();
        let mut properties = Properties::new();
        rec.for_each_field(&mut |key, val| {
            if let Some(prop) = key.strip_prefix(&prefix) {
                if !prop.starts_with("__") {
                    properties.insert(prop.to_string(), val.clone());
                }
            }
        });
        // Reconstruct labels from the colon-joined __label string.
        let labels: Vec<String> = if label_str.is_empty() {
            Vec::new()
        } else {
            label_str.split(':').map(|s| s.to_string()).collect()
        };
        return Some(Value::Node(Node {
            id: NodeId(id as u64),
            labels,
            properties,
        }));
    }

    None
}

/// Collect the set of variable names in a record that are bound as compound
/// entities (nodes or edges). Used by `RETURN *` to know which prefixes to
/// fold into compound columns rather than emitting as flat properties.
pub(crate) fn compound_binding_vars(rec: &dyn RecordView) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut vars: BTreeSet<String> = BTreeSet::new();
    rec.for_each_field(&mut |key, _| {
        if let Some((var, prop)) = key.split_once('.') {
            if prop == "__id" || prop == "__src" {
                vars.insert(var.to_string());
            }
        }
    });
    vars.into_iter().collect()
}

pub(in crate::cypher::executor) fn exec_project(
    conn: &Connection,
    input: &LogicalOp,
    items: &[crate::cypher::ast::ReturnItem],
    emit_compound: bool,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let mut projected = NamedRecord::new();
        for item in items {
            match &item.expr.kind {
                ExprKind::Star => {
                    if emit_compound {
                        // Final RETURN * — emit one compound column per bound
                        // variable (plus any non-binding user-visible scalars).
                        let bound_vars = compound_binding_vars(rec);
                        for var in &bound_vars {
                            if let Some(compound) = build_compound_binding(rec, var) {
                                projected.set(var.clone(), compound);
                            }
                        }
                        for (key, val) in &rec.fields {
                            if let Some((owner, prop)) = key.split_once('.') {
                                // Dotted key: skip internal fields and fields
                                // belonging to compound-bound variables.
                                if prop.starts_with("__") {
                                    continue;
                                }
                                if bound_vars.iter().any(|v| v == owner) {
                                    continue;
                                }
                                projected.set(key.clone(), val.clone());
                            } else {
                                // Bare key: emit if it's a scalar value from
                                // WITH/UNWIND (no accompanying __id metadata)
                                // and not already emitted as a compound binding.
                                if !bound_vars.iter().any(|v| v == key) {
                                    let has_id = rec.get(&format!("{key}.__id")).is_some();
                                    if !has_id {
                                        projected.set(key.clone(), val.clone());
                                    }
                                }
                            }
                        }
                    } else {
                        // Intermediate WITH * — preserve flat shape so
                        // downstream pattern-matching / joins / ORDER BY keep
                        // working against `var.__id` / `var.prop` fields.
                        for (key, val) in &rec.fields {
                            projected.set(key.clone(), val.clone());
                        }
                    }
                }
                ExprKind::Variable(var) => {
                    let col_name = item.alias.clone().unwrap_or_else(|| var.clone());
                    if emit_compound {
                        if let Some(compound) = build_compound_binding(rec, var) {
                            projected.set(col_name, compound);
                        } else if let Some(existing) = rec.get(&col_name) {
                            projected.set(col_name, existing.clone());
                        } else {
                            let val =
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                            projected.set(col_name, val);
                        }
                    } else {
                        // Intermediate: carry forward the flat binding shape so
                        // downstream operators can still access `var.prop` and
                        // `var.__id`. Rename prefixes when an alias was given.
                        let src_prefix = format!("{var}.");
                        let dst_prefix = format!("{col_name}.");
                        let mut propagated_any = false;
                        for (key, val) in &rec.fields {
                            if let Some(rest) = key.strip_prefix(&src_prefix) {
                                projected.set(format!("{dst_prefix}{rest}"), val.clone());
                                propagated_any = true;
                            }
                        }
                        // Also propagate the bare variable column if present
                        // (used by some operators as a compact id reference).
                        if let Some(existing) = rec.get(var) {
                            projected.set(col_name.clone(), existing.clone());
                            propagated_any = true;
                        }
                        if !propagated_any {
                            let val =
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
                            projected.set(col_name, val);
                        }
                    }
                }
                _ => {
                    let col_name = item
                        .alias
                        .clone()
                        .unwrap_or_else(|| expr_to_column_name(&item.expr));
                    let expr_col = expr_to_column_name(&item.expr);
                    // Check for deleted entity access before any cache lookup.
                    // Property access on a deleted entity must raise an error
                    // even if the value is still in the record.
                    if let ExprKind::Property(var, prop) = &item.expr.kind {
                        if rec.get(&format!("{var}.__deleted")) == Some(&Value::Bool(true)) {
                            return Err(GraphError::Query(
                                crate::types::QueryError::EntityNotFound {
                                    phase: crate::types::QueryPhase::Runtime,
                                    message: format!(
                                        "DeletedEntityAccess: cannot access property `{prop}` on deleted entity `{var}`"
                                    ),
                                    code: ErrorCode::Other,
                                    hint: None,
                                    span: None,
                                },
                            ));
                        }
                    }
                    // Check if the col_name collides with a MATCH variable binding
                    // (which stores raw node IDs). MATCH variables always have
                    // accompanying `var.__id` metadata; aggregate results don't.
                    let is_match_binding = rec.get(&format!("{col_name}.__id")).is_some();
                    let val = if !is_match_binding {
                        // First try the expression's natural column name — this
                        // is the key used by the Aggregate operator for group
                        // keys and the only safe name to match (it can never
                        // collide with an upstream variable).
                        if let Some(existing) = rec.get(&expr_col) {
                            existing.clone()
                        } else if col_name == expr_col {
                            // No alias rename — safe to check col_name too
                            // (already checked above, so this is a miss → eval).
                            eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                        } else if let Some(existing) = rec.get(&col_name) {
                            // Alias differs from expression name. The col_name
                            // value in the record may be a pre-computed aggregate
                            // result (stored under alias by agg_col_name) or a
                            // stale upstream variable. Aggregate results are
                            // stored under the alias, so check for that.
                            // Heuristic: if the expression is an aggregate
                            // function, trust the cached value; otherwise
                            // evaluate to avoid shadowing bugs.
                            let is_agg = matches!(
                                &item.expr.kind,
                                ExprKind::FunctionCall { name, .. }
                                    if matches!(name.to_ascii_lowercase().as_str(),
                                        "count" | "sum" | "avg" | "min" | "max" | "collect"
                                        | "percentiledisc" | "percentilecont" | "stdev" | "stdevp")
                            );
                            if is_agg {
                                existing.clone()
                            } else {
                                eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                            }
                        } else {
                            eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                        }
                    } else {
                        eval_expr(&item.expr, rec, crate::cypher::eval::EvalCx::new(conn))?
                    };
                    projected.set(col_name, val);
                }
            }
        }
        results.push(projected);
    }

    Ok(results)
}

/// Returns true if a record field should be visible to the user.
/// Filters out bare alias keys (no dot — raw node IDs) and internal `__` properties.
pub(crate) fn is_user_visible_field(key: &str) -> bool {
    match key.split_once('.') {
        Some((_, prop)) => !prop.starts_with("__"),
        None => false, // bare alias like "n" is the raw node ID — hide it
    }
}
pub(in crate::cypher::executor) fn exec_unwind(
    conn: &Connection,
    input: &LogicalOp,
    expr: &Expr,
    alias: &str,
    ctx: &ExecContext,
) -> Result<Vec<NamedRecord>> {
    let records = exec(conn, input, ctx)?;
    let mut results = Vec::new();

    for rec in &records {
        let val = eval_expr(expr, rec, crate::cypher::eval::EvalCx::new(conn))?;
        match val {
            Value::List(items) => {
                for item in items {
                    let mut new_rec = rec.clone();
                    // Expand nodes/edges into flat bindings for property access.
                    match &item {
                        Value::Node(n) => {
                            let node_rec = node_to_record(n, alias);
                            for (k, v) in &node_rec.fields {
                                new_rec.set(k.clone(), v.clone());
                            }
                        }
                        Value::Edge(e) => {
                            new_rec.set(alias.to_string(), Value::Edge(e.clone()));
                            new_rec.set(format!("{alias}.__src"), Value::I64(e.src.0 as i64));
                            new_rec.set(format!("{alias}.__dst"), Value::I64(e.dst.0 as i64));
                            new_rec.set(format!("{alias}.__type"), Value::String(e.label.clone()));
                            for (k, v) in &e.properties {
                                new_rec.set(format!("{alias}.{k}"), v.clone());
                            }
                        }
                        _ => {
                            new_rec.set(alias.to_string(), item);
                        }
                    }
                    results.push(new_rec);
                }
            }
            Value::Null => {
                // UNWIND null produces no rows (like UNWIND []).
            }
            _ => {
                return Err(GraphError::semantic(format!(
                    "UNWIND requires a list, got: {val}"
                )));
            }
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::ast::{Expr, ExprKind, LiteralValue};
    use crate::cypher::ir::FullTextOp;

    #[test]
    fn exec_fulltext_lookup_writes_fts_score_flat_key() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let mut props = std::collections::HashMap::new();
        props.insert(
            "body".to_string(),
            Value::String("rust systems programming".to_string()),
        );
        crate::storage::node::create_node(&conn, &["Doc".to_string()], props).unwrap();
        crate::storage::fts::create_fulltext_index(&conn, "Doc", "body").unwrap();

        let term_expr =
            Expr::synthetic(ExprKind::Literal(LiteralValue::String("rust".to_string())));
        let outer = NamedRecord::default();
        let records = exec_fulltext_lookup(
            &conn,
            "Doc",
            "n",
            "body",
            FullTextOp::Contains,
            false,
            &term_expr,
            None,
            &outer,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let score = records[0]
            .get("n.__fts_score")
            .expect("n.__fts_score flat key must be set");
        match score {
            Value::F64(s) => assert!(
                s.is_finite() && *s > 0.0,
                "score should be positive, got {s}"
            ),
            other => panic!("n.__fts_score was {other:?}, expected F64"),
        }
    }

    #[test]
    fn exec_fulltext_fallback_writes_zero_fts_score() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        let mut props = std::collections::HashMap::new();
        props.insert("body".to_string(), Value::String("ab cd".to_string()));
        crate::storage::node::create_node(&conn, &["Doc".to_string()], props).unwrap();
        crate::storage::fts::create_fulltext_index(&conn, "Doc", "body").unwrap();

        // Term length 2 is below the trigram floor → fallback path.
        let records = exec_fulltext_fallback(
            &conn,
            "Doc",
            "n",
            "body",
            FullTextOp::Contains,
            false,
            "ab",
            None,
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let score = records[0]
            .get("n.__fts_score")
            .expect("fallback must set n.__fts_score");
        assert_eq!(*score, Value::F64(0.0));
    }
}
