//! One source of truth for every read answer — shared by the CLI printers
//! and the MCP tools.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use fold::pipeline::terminal::search::Bm25Reader;
use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;

use crate::ident::normalize_node_id;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, NodeRec, StyleRec, VariableCollectionRec, VariableRec,
};

/// Read handle for the pipeline's `text` BM25 sink (its tokenizer type
/// param makes the full type unwieldy at every call site).
pub type TextReader<'tx, R> = Bm25Reader<'tx, R, String, fn(&str, &mut Vec<u8>)>;

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
        Some(raw) => normalize_node_id(&raw),
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

/// Full raw JSON of one node, optionally with a `children` summary array.
pub fn node<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    id: String,
    with_children: bool,
) -> Result<Value, String> {
    let id = normalize_node_id(&id);
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

    Ok(value)
}

/// Nodes by type, optionally within one page.
pub fn find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
) -> Result<Value, String> {
    // Figma node types are stored uppercase; normalize so `--type frame`
    // matches the same as `--type FRAME`.
    let mut ids = by_type.search(&node_type.to_uppercase());
    ids.sort();
    let page = page.as_deref().map(normalize_node_id);

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
        .map(|n| (n.id, n.name, n.page_id))
        .collect();
    rows.sort();

    let arr: Vec<Value> = rows
        .iter()
        .map(|(id, name, page_id)| json!({"id": id, "name": name, "page_id": page_id}))
        .collect();
    Ok(Value::Array(arr))
}

/// BM25 search over layer names and text content.
pub fn search<R: Readable>(
    text: &TextReader<'_, R>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    query: &str,
    limit: usize,
) -> Result<Value, String> {
    // BM25's own ranking order is deterministic; keep it (do not re-sort).
    let hits = text.search(query, limit);
    let rows: Vec<Value> = hits
        .iter()
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
    let target = normalize_node_id(target);
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
pub fn styles<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    style_type: Option<String>,
    values: bool,
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
                let value = consumers
                    .first()
                    .and_then(|nid| nodes.get(nid))
                    .and_then(|n| crate::vars::style_value_from_consumer(&s.style_type, &n.raw))
                    .unwrap_or(Value::Null);
                obj["value"] = value;
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
    let id = normalize_node_id(&id);
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
/// page, sorted by id.
pub fn text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
) -> Result<Value, String> {
    let mut ids = by_type.search(&"TEXT".to_string());
    ids.sort();
    let page = page.as_deref().map(normalize_node_id);

    let mut rows: Vec<(String, String, String)> = ids
        .into_iter()
        .filter_map(|id| nodes.get(&id))
        .filter(|n| page.as_deref().is_none_or(|p| n.page_id == p))
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
/// and/or scoped to one page. Rows `[{id, name, type, page_id, value}]`,
/// sorted by id.
pub fn where_<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    pointer: &str,
    equals: Option<Value>,
    page: Option<String>,
) -> Result<Value, String> {
    if !pointer.starts_with('/') {
        return Err(format!(
            "pointer must be an RFC 6901 pointer starting with '/': {pointer}"
        ));
    }
    let page = page.as_deref().map(normalize_node_id);

    let mut rows: Vec<(String, String, String, String, Value)> = Vec::new();
    for (_, n) in nodes.iter() {
        if !page.as_deref().is_none_or(|p| n.page_id == p) {
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
