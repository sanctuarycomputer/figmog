//! One source of truth for every read answer — shared by the CLI printers
//! and the MCP tools.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::{Value, json};

use fold::pipeline::terminal::search::Bm25Reader;
use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;

use crate::ident::normalize_node_ref;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, NodeRec, StyleRec, VariableCollectionRec, VariableRec,
};

/// Read handle for the pipeline's `text` BM25 sink (its tokenizer type
/// param makes the full type unwieldy at every call site).
pub type TextReader<'tx, R> = Bm25Reader<'tx, R, String, fn(&str, &mut Vec<u8>)>;

/// BFS the `children` index from `scope_root` (inclusive), collecting the
/// descendant id set — the shared engine behind `--under` scoping (spec
/// §3). Cycle-safe via the visited set, same discipline as `path`/
/// `depth_of`'s `parent_id` walks: a corrupted store with a `children`
/// cycle can't loop forever, it just stops re-queuing an id it's already
/// visited. Unknown `scope_root` is the caller's job to reject (via
/// `nodes.get` before calling this) so the error message names the right
/// argument.
fn scope_ids<R: Readable>(
    children: &MultimapReader<'_, R, String, (u32, String)>,
    scope_root: &str,
) -> BTreeSet<String> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    visited.insert(scope_root.to_string());
    queue.push_back(scope_root.to_string());
    while let Some(current) = queue.pop_front() {
        for (_, child_id) in children.get(&current) {
            if visited.insert(child_id.clone()) {
                queue.push_back(child_id);
            }
        }
    }
    visited
}

/// Resolve an optional `--under <id>` scope argument to the descendant id
/// set it names (root inclusive), or `None` when no scope was requested.
/// Unknown id ⇒ the standard "no node … in the mirror" error (spec §3).
fn resolve_under<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    under: Option<String>,
) -> Result<Option<BTreeSet<String>>, String> {
    match under {
        None => Ok(None),
        Some(raw) => {
            let id = normalize_node_ref(&raw);
            nodes
                .get(&id)
                .ok_or_else(|| format!("no node {id} in the mirror"))?;
            Ok(Some(scope_ids(children, &id)))
        }
    }
}

/// One `boundVariables` binding site resolved against the variables table
/// (spec §6): `{pointer, variable_id, variable_name, values_by_mode}` when
/// the variable is known (imported or Enterprise-synced), else
/// `{pointer, variable_id, source: "unresolved"}`. Never errors — an
/// unresolved binding is a normal, expected outcome on the free plan.
fn resolve_variable_binding<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    pointer: &str,
    variable_id: &str,
) -> Value {
    match variables.get(&variable_id.to_string()) {
        Some(var) => {
            let collection = variable_collections.get(&var.collection_id);
            let mut values_by_mode = serde_json::Map::new();
            for (mode_id, val_str) in &var.values_by_mode {
                let mode_name = collection
                    .as_ref()
                    .and_then(|c| c.modes.iter().find(|(mid, _)| mid == mode_id))
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| mode_id.clone());
                let val: Value = serde_json::from_str(val_str).unwrap_or(Value::Null);
                values_by_mode.insert(mode_name, val);
            }
            json!({
                "pointer": pointer,
                "variable_id": variable_id,
                "variable_name": var.name,
                "values_by_mode": Value::Object(values_by_mode),
            })
        }
        None => json!({
            "pointer": pointer,
            "variable_id": variable_id,
            "source": "unresolved",
        }),
    }
}

/// `resolved_variables` array for a node's (or a style's) full set of
/// `boundVariables` binding sites, in the sorted order `bound_variables`
/// already carries (model.rs's determinism contract) — never re-sorted.
fn resolved_variables<R: Readable>(
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    bound_variables: &[(String, String)],
) -> Value {
    let arr: Vec<Value> = bound_variables
        .iter()
        .map(|(pointer, vid)| {
            resolve_variable_binding(variables, variable_collections, pointer, vid)
        })
        .collect();
    Value::Array(arr)
}

/// File name, version, last modified, node count.
pub fn status<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
) -> Result<Value, String> {
    let m = meta
        .get(&0)
        .ok_or_else(|| "no mirror here — run `figmog pull <file-url>` first".to_string())?;
    let count = nodes.iter().count();
    Ok(json!({
        "name": m.name,
        "version": m.version,
        "last_modified": m.last_modified,
        "synced_at_unix_ms": m.synced_at_unix_ms,
        "nodes": count,
    }))
}

/// List pages, ordered by document child index.
pub fn pages<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<Value, String> {
    let mut ids = by_type.search(&"CANVAS".to_string());
    ids.sort();

    let mut rows: Vec<(u32, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id).map(|n| (n.child_index, n.id, n.name)))
        .collect();
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));

    let arr: Vec<Value> = rows
        .iter()
        .map(|(_, id, name)| json!({"id": id, "name": name}))
        .collect();
    Ok(Value::Array(arr))
}

/// One level of a `tree` outline; JSON shape `{id, name, type, children}`.
pub struct TreeNode {
    pub id: String,
    pub name: String,
    pub node_type: String,
    pub children: Vec<TreeNode>,
}

pub fn build_tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    node: &NodeRec,
    depth: Option<usize>,
) -> TreeNode {
    let mut kids = Vec::new();
    if depth != Some(0) {
        let mut edges = children.get(&node.id);
        edges.sort();
        let next_depth = depth.map(|d| d - 1);
        for (_, child_id) in edges {
            if let Some(child) = nodes.get(&child_id) {
                kids.push(build_tree(nodes, children, &child, next_depth));
            }
        }
    }
    TreeNode {
        id: node.id.clone(),
        name: node.name.clone(),
        node_type: node.node_type.clone(),
        children: kids,
    }
}

fn tree_to_json(t: &TreeNode) -> Value {
    json!({
        "id": t.id,
        "name": t.name,
        "type": t.node_type,
        "children": t.children.iter().map(tree_to_json).collect::<Vec<_>>(),
    })
}

/// Resolve the root (default: the DOCUMENT node) and build its outline as a
/// [`TreeNode`], so callers that render for humans can walk the same
/// structure `tree`'s JSON is built from.
pub fn tree_nodes<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<TreeNode, String> {
    let start = match id {
        Some(raw) => normalize_node_ref(&raw),
        None => {
            let mut docs = by_type.search(&"DOCUMENT".to_string());
            docs.sort();
            docs.into_iter()
                .next()
                .ok_or_else(|| "no DOCUMENT node in the mirror".to_string())?
        }
    };
    let root = nodes
        .get(&start)
        .ok_or_else(|| format!("no node {start} in the mirror"))?;
    Ok(build_tree(nodes, children, &root, depth))
}

/// Subtree outline (default: whole document).
pub fn tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<Value, String> {
    let t = tree_nodes(nodes, children, by_type, id, depth)?;
    Ok(tree_to_json(&t))
}

/// Full raw JSON of one node, optionally with a `children` summary array
/// and/or a `resolved_variables` annotation (spec §6) of every
/// `boundVariables` binding site on this node.
pub fn node<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    with_children: bool,
    resolve_vars: bool,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let n = nodes
        .get(&id)
        .ok_or_else(|| format!("no node {id} in the mirror"))?;
    let mut value: Value = serde_json::from_str(&n.raw).map_err(|e| e.to_string())?;

    if with_children {
        let mut edges = children.get(&id);
        edges.sort();
        let kids: Vec<Value> = edges
            .into_iter()
            .filter_map(|(_, child_id)| {
                nodes
                    .get(&child_id)
                    .map(|n| json!({"id": n.id, "name": n.name, "type": n.node_type}))
            })
            .collect();
        if let Some(obj) = value.as_object_mut() {
            obj.insert("children".to_string(), Value::Array(kids));
        }
    }

    if resolve_vars && let Some(obj) = value.as_object_mut() {
        obj.insert(
            "resolved_variables".to_string(),
            resolved_variables(variables, variable_collections, &n.bound_variables),
        );
    }

    Ok(value)
}

/// Top-level raw fields a `fields` projection keeps regardless of whether
/// they were named (spec §2): `id`/`name`/`type` because a projected node
/// must stay identifiable. `children` isn't listed here — `raw` never
/// carries a `children` field (flatten strips it), and [`build_subtree`]
/// always adds it back after projection, so it survives unconditionally
/// without needing a name-check.
const SUBTREE_ALWAYS_FIELDS: [&str; 3] = ["id", "name", "type"];

/// Project a subtree node's raw JSON object down to `fields` plus the
/// always-kept set. Unknown field names are simply absent, not an error —
/// spec §2's "agents probe freely" contract. A non-object `value` (should
/// never happen for a real Figma node) passes through unchanged.
fn project_raw_fields(mut value: Value, fields: &[String]) -> Value {
    if let Value::Object(ref mut map) = value {
        map.retain(|k, _| {
            SUBTREE_ALWAYS_FIELDS.contains(&k.as_str()) || fields.iter().any(|f| f == k)
        });
    }
    value
}

/// Recursive worker behind [`subtree`]: raw JSON of `n`, fields-projected,
/// with `children` nested in child-index order and (when requested)
/// `resolved_variables` — both inserted *after* projection so they always
/// survive a `fields` filter, matching `id`/`name`/`type`.
#[allow(clippy::too_many_arguments)]
fn build_subtree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    n: &NodeRec,
    depth: Option<usize>,
    fields: Option<&[String]>,
    resolve_vars: bool,
) -> Result<Value, String> {
    let mut value: Value = serde_json::from_str(&n.raw).map_err(|e| e.to_string())?;
    if let Some(fs) = fields {
        value = project_raw_fields(value, fs);
    }

    let mut kids_json = Vec::new();
    if depth != Some(0) {
        let mut edges = children.get(&n.id);
        edges.sort();
        let next_depth = depth.map(|d| d - 1);
        for (_, child_id) in edges {
            if let Some(child) = nodes.get(&child_id) {
                kids_json.push(build_subtree(
                    nodes,
                    children,
                    variables,
                    variable_collections,
                    &child,
                    next_depth,
                    fields,
                    resolve_vars,
                )?);
            }
        }
    }

    if let Some(obj) = value.as_object_mut() {
        obj.insert("children".to_string(), Value::Array(kids_json));
        if resolve_vars {
            obj.insert(
                "resolved_variables".to_string(),
                resolved_variables(variables, variable_collections, &n.bound_variables),
            );
        }
    }

    Ok(value)
}

/// Subtree dump rooted at `id` (spec §2): the node's full raw JSON with
/// `children: [...]` nested recursively in child-index order, to `depth`
/// levels (default: unlimited). `fields` projects every node to the named
/// raw fields (`id`/`name`/`type`/`children` always survive); `None` skips
/// projection entirely. `resolve_vars` adds a `resolved_variables`
/// annotation (spec §6) to every node.
#[allow(clippy::too_many_arguments)]
pub fn subtree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: String,
    depth: Option<usize>,
    fields: Option<&[String]>,
    resolve_vars: bool,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let root = nodes
        .get(&id)
        .ok_or_else(|| format!("no node {id} in the mirror"))?;
    build_subtree(
        nodes,
        children,
        variables,
        variable_collections,
        &root,
        depth,
        fields,
        resolve_vars,
    )
}

/// Nodes by type, optionally within one page and/or scoped to `under`'s
/// subtree (spec §3: intersects with the page filter). `page` and `under`
/// both accept a full Figma URL (spec §2b), same as any other node-id
/// argument.
pub fn find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    // Figma node types are stored uppercase; normalize so `--type frame`
    // matches the same as `--type FRAME`.
    let mut ids = by_type.search(&node_type.to_uppercase());
    ids.sort();
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
        .filter(|n| scope.as_ref().is_none_or(|s| s.contains(&n.id)))
        .map(|n| (n.id, n.name, n.page_id))
        .collect();
    rows.sort();

    let arr: Vec<Value> = rows
        .iter()
        .map(|(id, name, page_id)| json!({"id": id, "name": name, "page_id": page_id}))
        .collect();
    Ok(Value::Array(arr))
}

/// BM25 search over layer names and text content, optionally scoped to
/// `under`'s subtree (spec §3). Scoped calls rank the *entire* matching
/// corpus before filtering: `Bm25Reader::search` already scores every
/// matching document internally regardless of `limit` (its own `limit`
/// only truncates the final sorted list), so passing an effectively
/// unlimited `limit` here isn't a more expensive query — it just skips
/// that final truncation until after the scope filter runs, so an
/// out-of-scope top-k match can't crowd an in-scope one out of the
/// response. Unscoped calls keep the direct top-`limit` path unchanged.
pub fn search<R: Readable>(
    text: &TextReader<'_, R>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    query: &str,
    limit: usize,
    under: Option<String>,
) -> Result<Value, String> {
    let scope = resolve_under(nodes, children, under)?;
    // BM25's own ranking order is deterministic; keep it (do not re-sort).
    let search_limit = if scope.is_some() { usize::MAX } else { limit };
    let hits = text.search(query, search_limit);
    let rows: Vec<Value> = hits
        .iter()
        .filter(|hit| scope.as_ref().is_none_or(|s| s.contains(&hit.val)))
        .take(limit)
        .filter_map(|hit| {
            let node = nodes.get(&hit.val)?;
            let snippet = node
                .text
                .as_ref()
                .map(|t| t.chars().take(80).collect::<String>());
            Some(json!({
                "id": node.id,
                "score": hit.score,
                "type": node.node_type,
                "name": node.name,
                "page_id": node.page_id,
                "snippet": snippet,
            }))
        })
        .collect();
    Ok(Value::Array(rows))
}

/// Resolve a target (node id, component key, or component/set name) to the
/// component node ids it names, in priority order: exact node id, then key,
/// then set name (all variants), then component name (all matches).
fn resolve_component_ids<R: Readable>(
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    target: &str,
) -> Vec<String> {
    if components.contains(&target.to_string()) {
        return vec![target.to_string()];
    }

    let mut ids: Vec<String> = components
        .iter()
        .filter(|(_, c)| c.key == target)
        .map(|(id, _)| id)
        .collect();
    if !ids.is_empty() {
        return ids;
    }

    let set_ids: Vec<String> = component_sets
        .iter()
        .filter(|(_, s)| s.name == target)
        .map(|(id, _)| id)
        .collect();
    if !set_ids.is_empty() {
        ids = components
            .iter()
            .filter(|(_, c)| {
                c.component_set_id
                    .as_deref()
                    .is_some_and(|s| set_ids.iter().any(|sid| sid == s))
            })
            .map(|(id, _)| id)
            .collect();
        return ids;
    }

    components
        .iter()
        .filter(|(_, c)| c.name == target)
        .map(|(id, _)| id)
        .collect()
}

/// Instances of a component (by node id, key, or name).
pub fn instances<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    instances_of: &InvertedIndexReader<'_, R, String, String>,
    target: &str,
) -> Result<Value, String> {
    let target = normalize_node_ref(target);
    let component_ids = resolve_component_ids(components, component_sets, &target);

    let mut instance_ids: BTreeSet<String> = BTreeSet::new();
    for cid in &component_ids {
        instance_ids.extend(instances_of.search(cid));
    }

    let rows: Vec<Value> = instance_ids
        .iter()
        .filter_map(|id| nodes.get(id))
        .map(|n| json!({"id": n.id, "name": n.name, "page_id": n.page_id, "component_id": n.component_id}))
        .collect();
    Ok(Value::Array(rows))
}

/// Design-system inventory: sets, variant axes, standalone components.
pub fn components<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
) -> Result<Value, String> {
    let mut sets: Vec<(String, ComponentSetRec)> = component_sets.iter().collect();
    sets.sort_by(|a, b| a.0.cmp(&b.0));

    let mut all_components: Vec<(String, ComponentRec)> = components.iter().collect();
    all_components.sort_by(|a, b| a.0.cmp(&b.0));

    let sets_json: Vec<Value> = sets
        .iter()
        .map(|(set_id, set)| {
            let variants: Vec<Value> = all_components
                .iter()
                .filter(|(_, c)| c.component_set_id.as_deref() == Some(set_id.as_str()))
                .map(|(cid, c)| json!({"node_id": cid, "name": c.name, "key": c.key}))
                .collect();
            let property_definitions: Value = nodes
                .get(set_id)
                .and_then(|n| n.property_definitions.clone())
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(Value::Null);
            json!({
                "node_id": set_id,
                "name": set.name,
                "key": set.key,
                "variants": variants,
                "property_definitions": property_definitions,
            })
        })
        .collect();

    let standalone: Vec<Value> = all_components
        .iter()
        .filter(|(_, c)| c.component_set_id.is_none())
        .map(|(cid, c)| json!({"node_id": cid, "name": c.name, "key": c.key}))
        .collect();

    Ok(json!({"sets": sets_json, "components": standalone}))
}

/// Styles with usage counts; `values` derives definitions from consumers.
/// `resolve_vars` (with `values`) adds a `resolved_variables` annotation
/// (spec §6) of the variable bindings under that definition's own raw-JSON
/// pointer prefix (e.g. a FILL style's bindings live under `/fills`) — a
/// no-op without `values`, since there's no definition to annotate.
#[allow(clippy::too_many_arguments)]
pub fn styles<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    style_type: Option<String>,
    values: bool,
    resolve_vars: bool,
) -> Result<Value, String> {
    let mut rows: Vec<(String, StyleRec)> = styles.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(t) = &style_type {
        rows.retain(|(_, s)| s.style_type.eq_ignore_ascii_case(t));
    }

    let out: Vec<Value> = rows
        .iter()
        .map(|(style_id, s)| {
            let mut consumers = styled_by.search(style_id);
            consumers.sort();
            let mut obj = json!({
                "style_id": style_id,
                "name": s.name,
                "key": s.key,
                "type": s.style_type,
                "uses": consumers.len(),
            });
            if values {
                let consumer = consumers.first().and_then(|nid| nodes.get(nid));
                let value = consumer
                    .as_ref()
                    .and_then(|n| crate::vars::style_value_from_consumer(&s.style_type, &n.raw))
                    .unwrap_or(Value::Null);
                obj["value"] = value;

                if resolve_vars {
                    let prefix = crate::vars::style_value_pointer(&s.style_type);
                    let bound: Vec<(String, String)> = consumer
                        .map(|n| {
                            n.bound_variables
                                .iter()
                                .filter(|(p, _)| prefix.is_some_and(|pre| p.starts_with(pre)))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    obj["resolved_variables"] =
                        resolved_variables(variables, variable_collections, &bound);
                }
            }
            obj
        })
        .collect();
    Ok(Value::Array(out))
}

/// Nodes using a style id or bound to a variable id.
pub fn uses<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    bound_to: &InvertedIndexReader<'_, R, String, String>,
    id: &str,
) -> Result<Value, String> {
    let id = id.to_string();
    let mut ids = styled_by.search(&id);
    if ids.is_empty() {
        ids = bound_to.search(&id);
    }
    ids.sort();

    let rows: Vec<Value> = ids
        .iter()
        .filter_map(|nid| nodes.get(nid))
        .map(|n| json!({"id": n.id, "name": n.name, "page_id": n.page_id}))
        .collect();
    Ok(Value::Array(rows))
}

/// Variables: authoritative if imported, else inferred from bindings.
pub fn vars<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id_filter: Option<String>,
) -> Result<Value, String> {
    let owned_nodes: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();
    let inferred = crate::vars::infer_from_nodes(owned_nodes.iter());
    let mut inferred_by_id: HashMap<String, crate::vars::VarUsage> = inferred
        .into_iter()
        .map(|u| (u.variable_id.clone(), u))
        .collect();

    let mut all_ids: BTreeSet<String> = inferred_by_id.keys().cloned().collect();
    all_ids.extend(variables.iter().map(|(k, _)| k));
    if let Some(target) = &id_filter {
        all_ids.retain(|v| v == target);
    }

    let rows: Vec<Value> = all_ids
        .iter()
        .map(|vid| {
            let usage = inferred_by_id.remove(vid);
            let (sites, observed) = usage.map(|u| (u.sites, u.observed)).unwrap_or_default();

            if let Some(var) = variables.get(vid) {
                let collection = variable_collections.get(&var.collection_id);
                let mut values_by_mode = serde_json::Map::new();
                for (mode_id, val_str) in &var.values_by_mode {
                    let mode_name = collection
                        .as_ref()
                        .and_then(|c| c.modes.iter().find(|(mid, _)| mid == mode_id))
                        .map(|(_, name)| name.clone())
                        .unwrap_or_else(|| mode_id.clone());
                    let val: Value = serde_json::from_str(val_str).unwrap_or(Value::Null);
                    values_by_mode.insert(mode_name, val);
                }
                json!({
                    "variable_id": vid,
                    "source": "imported",
                    "name": var.name,
                    "resolved_type": var.resolved_type,
                    "collection": collection.map(|c| c.name),
                    "values_by_mode": Value::Object(values_by_mode),
                    "sites": sites,
                    "observed": observed,
                })
            } else {
                json!({
                    "variable_id": vid,
                    "source": "inferred",
                    "sites": sites,
                    "observed": observed,
                })
            }
        })
        .collect();
    Ok(Value::Array(rows))
}

// ---- whole-file structural queries ----
//
// The local mirror's unfair advantage: full-file scans/joins no
// rate-limited API surface could offer, all answered from the local store.

/// Depth of `id` counting the root as 0, by walking `parent_id` up to the
/// root. The file is local, so an O(depth) walk per node is fine. Guards
/// against a corrupted store with a `parent_id` cycle: once an id repeats,
/// stop and report the depth counted so far rather than looping forever —
/// this runs inside `figmog serve`'s single-threaded loop, where a hang
/// would stall the whole server.
fn depth_of<R: Readable>(nodes: &TableReader<'_, R, String, NodeRec>, id: &str) -> usize {
    let mut depth = 0;
    let mut current = id.to_string();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    while let Some(n) = nodes.get(&current) {
        if !visited.insert(current.clone()) {
            break; // parent cycle: depth-so-far is the best available answer
        }
        match n.parent_id {
            Some(parent) => {
                depth += 1;
                current = parent;
            }
            None => break,
        }
    }
    depth
}

/// Node counts by type and by page, table totals, text-node count, max tree
/// depth.
#[allow(clippy::too_many_arguments)]
pub fn stats<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<Value, String> {
    let all: Vec<NodeRec> = nodes.iter().map(|(_, n)| n).collect();

    let mut by_type_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_page_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut max_depth = 0usize;
    for n in &all {
        *by_type_counts.entry(n.node_type.clone()).or_insert(0) += 1;
        // DOCUMENT/CANVAS nodes are pages/roots, not page contents.
        if n.node_type != "DOCUMENT" && n.node_type != "CANVAS" {
            *by_page_counts.entry(n.page_id.clone()).or_insert(0) += 1;
        }
        max_depth = max_depth.max(depth_of(nodes, &n.id));
    }

    let text_nodes = by_type.search(&"TEXT".to_string()).len();

    Ok(json!({
        "by_type": by_type_counts,
        "by_page": by_page_counts,
        "totals": {
            "components": components.iter().count(),
            "component_sets": component_sets.iter().count(),
            "styles": styles.iter().count(),
            "variables": variables.iter().count(),
        },
        "text_nodes": text_nodes,
        "max_depth": max_depth,
    }))
}

/// Ancestor chain root→node, as `[{id, name, type}]`. Unknown id → Err.
/// A `parent_id` cycle (a corrupted store) is also an `Err` rather than an
/// infinite loop — see `depth_of`'s doc comment for why that matters here.
pub fn path<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    id: String,
) -> Result<Value, String> {
    let id = normalize_node_ref(&id);
    let mut chain: Vec<NodeRec> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut current = id.clone();
    loop {
        if !visited.insert(current.clone()) {
            return Err(format!("parent cycle detected at {current}"));
        }
        let n = nodes
            .get(&current)
            .ok_or_else(|| format!("no node {current} in the mirror"))?;
        let parent = n.parent_id.clone();
        chain.push(n);
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    chain.reverse();

    let arr: Vec<Value> = chain
        .iter()
        .map(|n| json!({"id": n.id, "name": n.name, "type": n.node_type}))
        .collect();
    Ok(Value::Array(arr))
}

/// Every TEXT node's `(id, characters, page_id)`, optionally scoped to one
/// page and/or `under`'s subtree (spec §3: intersects with the page
/// filter), sorted by id. `page` and `under` both accept a full Figma URL
/// (spec §2b), same as any other node-id argument.
pub fn text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    let mut ids = by_type.search(&"TEXT".to_string());
    ids.sort();
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
        .filter(|n| scope.as_ref().is_none_or(|s| s.contains(&n.id)))
        .map(|n| (n.id, n.text.clone().unwrap_or_default(), n.page_id))
        .collect();
    rows.sort();

    let arr: Vec<Value> = rows
        .iter()
        .map(|(id, characters, page_id)| {
            json!({"id": id, "characters": characters, "page_id": page_id})
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Nodes whose `raw` JSON matches an RFC 6901 pointer, optionally by value
/// and/or scoped to one page and/or `under`'s subtree (spec §3: intersects
/// with the page filter). Rows `[{id, name, type, page_id, value}]`,
/// sorted by id. `page` and `under` both accept a full Figma URL (spec
/// §2b), same as any other node-id argument.
pub fn where_<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    pointer: &str,
    equals: Option<Value>,
    page: Option<String>,
    under: Option<String>,
) -> Result<Value, String> {
    if !pointer.starts_with('/') {
        return Err(format!(
            "pointer must be an RFC 6901 pointer starting with '/': {pointer}"
        ));
    }
    let page = page.as_deref().map(normalize_node_ref);
    let scope = resolve_under(nodes, children, under)?;

    let mut rows: Vec<(String, String, String, String, Value)> = Vec::new();
    for (_, n) in nodes.iter() {
        if !page.as_deref().is_none_or(|p| n.page_id == p) {
            continue;
        }
        if !scope.as_ref().is_none_or(|s| s.contains(&n.id)) {
            continue;
        }
        let raw: Value = serde_json::from_str(&n.raw).map_err(|e| e.to_string())?;
        let Some(v) = raw.pointer(pointer) else {
            continue;
        };
        if equals.as_ref().is_some_and(|want| v != want) {
            continue;
        }
        rows.push((n.id, n.name, n.node_type, n.page_id, v.clone()));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let arr: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, node_type, page_id, value)| {
            json!({"id": id, "name": name, "type": node_type, "page_id": page_id, "value": value})
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Nodes whose `abs_bounds` contain `(x, y)`, sorted by area ascending
/// (deepest/smallest first) then id. Rows `[{id, name, type, page_id, area}]`.
pub fn at<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    x: f64,
    y: f64,
) -> Result<Value, String> {
    let mut rows: Vec<(f64, String, String, String, String)> = Vec::new();
    for (_, n) in nodes.iter() {
        let Some([bx, by, w, h]) = n.abs_bounds else {
            continue;
        };
        if bx <= x && x < bx + w && by <= y && y < by + h {
            rows.push((w * h, n.id, n.name, n.node_type, n.page_id));
        }
    }
    rows.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    let arr: Vec<Value> = rows
        .into_iter()
        .map(|(area, id, name, node_type, page_id)| {
            json!({"id": id, "name": name, "type": node_type, "page_id": page_id, "area": area})
        })
        .collect();
    Ok(Value::Array(arr))
}
