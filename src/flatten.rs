//! Pure flattening of a Figma file response into deterministic records.
//!
//! No I/O, no clock, no randomness: two calls on equal JSON must produce
//! byte-identical records (postcard), because `KeyedStream::upsert` uses
//! byte equality as its change detector.

use serde_json::Value;
use std::collections::BTreeMap;

use crate::model::{ComponentRec, ComponentSetRec, Id, NodeRec, Rec, StyleRec};

/// File-level fields lifted from the response envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub version: String,
    pub last_modified: String,
}

/// Everything `flatten_file` extracts.
#[derive(Debug)]
pub struct Flattened {
    pub recs: Vec<(Id, Rec)>,
    pub file: FileInfo,
}

/// Errors from [`flatten_file`].
#[derive(Debug, thiserror::Error)]
pub enum FlattenError {
    #[error("missing field: {0}")]
    Missing(&'static str),
}

/// Flatten a full `GET /v1/files/:key` response.
pub fn flatten_file(resp: &Value) -> Result<Flattened, FlattenError> {
    let file = FileInfo {
        name: str_field(resp, "name").ok_or(FlattenError::Missing("name"))?,
        version: str_field(resp, "version").ok_or(FlattenError::Missing("version"))?,
        last_modified: str_field(resp, "lastModified")
            .ok_or(FlattenError::Missing("lastModified"))?,
    };
    let document = resp
        .get("document")
        .ok_or(FlattenError::Missing("document"))?;

    let mut recs = Vec::new();
    walk(document, None, 0, None, &mut recs);

    // Borrowed, not cloned: every entry is read once here (a handful of
    // `str_field`/`get` lookups) and dropped, so there's no reason to clone
    // each envelope `Value` just to iterate it. `BTreeMap` keeps the same
    // deterministic (sorted-by-id) iteration order as before.
    let obj_map = |key: &str| -> BTreeMap<&str, &Value> {
        resp.get(key)
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
            .unwrap_or_default()
    };
    for (node_id, v) in obj_map("components") {
        recs.push((
            Id::Component(node_id.to_string()),
            Rec::Component(ComponentRec {
                node_id: node_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                component_set_id: str_field(v, "componentSetId"),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }
    for (node_id, v) in obj_map("componentSets") {
        recs.push((
            Id::ComponentSet(node_id.to_string()),
            Rec::ComponentSet(ComponentSetRec {
                node_id: node_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }
    for (style_id, v) in obj_map("styles") {
        recs.push((
            Id::Style(style_id.to_string()),
            Rec::Style(StyleRec {
                style_id: style_id.to_string(),
                key: str_field(v, "key").unwrap_or_default(),
                name: str_field(v, "name").unwrap_or_default(),
                style_type: str_field(v, "styleType").unwrap_or_default(),
                description: str_field(v, "description").unwrap_or_default(),
                remote: v.get("remote").and_then(Value::as_bool).unwrap_or(false),
            }),
        ));
    }

    Ok(Flattened { recs, file })
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(str::to_string)
}

/// Depth-first walk. `page_id` is the nearest CANVAS ancestor (None above
/// pages — the record then carries the node's own id).
fn walk(
    node: &Value,
    parent_id: Option<&str>,
    child_index: u32,
    page_id: Option<&str>,
    out: &mut Vec<(Id, Rec)>,
) {
    let Some(id) = node.get("id").and_then(Value::as_str) else {
        return; // node without id: skip it and its subtree
    };
    let node_type = node
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_string();
    let own_page = matches!(node_type.as_str(), "DOCUMENT" | "CANVAS");
    let page = if own_page { id } else { page_id.unwrap_or(id) };

    let mut raw = node.clone();
    if let Some(obj) = raw.as_object_mut() {
        obj.remove("children");
    }

    let bound_variables = scan_bound_variables(&raw);

    let rec = NodeRec {
        id: id.to_string(),
        parent_id: parent_id.map(str::to_string),
        child_index,
        page_id: page.to_string(),
        node_type,
        name: str_field(node, "name").unwrap_or_default(),
        visible: node.get("visible").and_then(Value::as_bool).unwrap_or(true),
        text: str_field(node, "characters"),
        component_id: str_field(node, "componentId"),
        component_properties: sorted_map(node.get("componentProperties"), |v| {
            v.get("value")
                .map(|val| serde_json::to_string(val).expect("Value serializes"))
        }),
        property_definitions: node
            .get("componentPropertyDefinitions")
            .map(|v| serde_json::to_string(v).expect("Value serializes")),
        style_refs: sorted_map(node.get("styles"), |v| v.as_str().map(str::to_string)),
        bound_variables,
        abs_bounds: node.get("absoluteBoundingBox").and_then(|b| {
            Some([
                b.get("x")?.as_f64()?,
                b.get("y")?.as_f64()?,
                b.get("width")?.as_f64()?,
                b.get("height")?.as_f64()?,
            ])
        }),
        raw: serde_json::to_string(&raw).expect("serde_json::Value serializes"),
    };
    out.push((Id::Node(id.to_string()), Rec::Node(rec)));

    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for (i, child) in children.iter().enumerate() {
            walk(child, Some(id), i as u32, Some(page), out);
        }
    }
}

/// Turn a JSON object into sorted (key, f(value)) pairs; absent/None entries drop.
fn sorted_map(obj: Option<&Value>, f: impl Fn(&Value) -> Option<String>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = obj
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), f(v)?)))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Recursively find every `boundVariables` object and emit
/// (pointer-to-resolved-value, variable id) pairs. The binding
/// `…/boundVariables/<prop> = {type: VARIABLE_ALIAS, id}` resolves at the
/// sibling `…/<prop>`, which is where Figma bakes the concrete value.
fn scan_bound_variables(node_raw: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    scan_bv(node_raw, "", &mut out);
    out.sort();
    out.dedup();
    out
}

fn scan_bv(v: &Value, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == "boundVariables" {
                    collect_aliases(child, path, out);
                } else {
                    scan_bv(child, &format!("{path}/{k}"), out);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                scan_bv(child, &format!("{path}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// Walk the *inside* of a `boundVariables` object: values are aliases,
/// arrays of aliases, or nested objects of them.
fn collect_aliases(v: &Value, prop_path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) => {
            let alias = map.get("type").and_then(Value::as_str) == Some("VARIABLE_ALIAS");
            if alias && let Some(id) = map.get("id").and_then(Value::as_str) {
                out.push((prop_path.to_string(), id.to_string()));
                return;
            }
            for (k, child) in map {
                collect_aliases(child, &format!("{prop_path}/{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                collect_aliases(child, &format!("{prop_path}/{i}"), out);
            }
        }
        _ => {}
    }
}
