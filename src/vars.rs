//! Variables: authoritative import parsing (this module also hosts the
//! free-plan inference in [`infer_from_nodes`]).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::model::{Id, NodeRec, Rec, VariableCollectionRec, VariableRec};

/// Errors from [`parse_variables_export`].
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("unrecognized variables export shape: {0}")]
    Shape(String),
}

/// Parse a variables export: either the Enterprise REST `variables/local`
/// response (`{meta: {variables, variableCollections}}`) or the bare
/// object a plugin-console export produces.
pub fn parse_variables_export(v: &Value) -> Result<Vec<(Id, Rec)>, ImportError> {
    let root = v.get("meta").unwrap_or(v);
    let variables = root
        .get("variables")
        .and_then(Value::as_object)
        .ok_or_else(|| ImportError::Shape("missing `variables` object".into()))?;
    let collections = root
        .get("variableCollections")
        .and_then(Value::as_object)
        .ok_or_else(|| ImportError::Shape("missing `variableCollections` object".into()))?;

    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let mut recs = Vec::new();
    let sorted: BTreeMap<_, _> = collections.iter().collect();
    for (id, c) in sorted {
        let mut modes: Vec<(String, String)> = c
            .get("modes")
            .and_then(Value::as_array)
            .map(|ms| ms.iter().map(|m| (s(m, "modeId"), s(m, "name"))).collect())
            .unwrap_or_default();
        modes.sort();
        recs.push((
            Id::VariableCollection(id.clone()),
            Rec::VariableCollection(VariableCollectionRec {
                id: id.clone(),
                name: s(c, "name"),
                modes,
                default_mode_id: s(c, "defaultModeId"),
            }),
        ));
    }
    let sorted: BTreeMap<_, _> = variables.iter().collect();
    for (id, var) in sorted {
        let mut values_by_mode: Vec<(String, String)> = var
            .get("valuesByMode")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(mode, val)| {
                        (
                            mode.clone(),
                            serde_json::to_string(val).expect("Value serializes"),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        values_by_mode.sort();
        let scopes: Vec<String> = var
            .get("scopes")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        recs.push((
            Id::Variable(id.clone()),
            Rec::Variable(VariableRec {
                id: id.clone(),
                name: s(var, "name"),
                resolved_type: s(var, "resolvedType"),
                collection_id: s(var, "variableCollectionId"),
                values_by_mode,
                description: s(var, "description"),
                scopes,
            }),
        ));
    }
    Ok(recs)
}

/// Everything known about one variable from its usage sites alone.
#[derive(Debug, Serialize)]
pub struct VarUsage {
    pub variable_id: String,
    /// (node_id, json-pointer of the bound property), sorted.
    pub sites: Vec<(String, String)>,
    /// Distinct resolved values observed at those sites (canonical JSON),
    /// sorted. Usually one value; more indicates multi-mode usage.
    pub observed: Vec<String>,
}

/// Free-plan inference: fold every node's variable bindings into per-variable
/// usage + observed resolved values (the concrete values Figma bakes in
/// next to each binding — default-mode values unless a frame overrides its
/// mode).
pub fn infer_from_nodes<'a>(nodes: impl Iterator<Item = &'a NodeRec>) -> Vec<VarUsage> {
    type VarData = (Vec<(String, String)>, Vec<String>);
    let mut by_var: BTreeMap<String, VarData> = BTreeMap::new();
    for node in nodes {
        let raw: serde_json::Value = match serde_json::from_str(&node.raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (pointer, var_id) in &node.bound_variables {
            let entry = by_var.entry(var_id.clone()).or_default();
            entry.0.push((node.id.clone(), pointer.clone()));
            if let Some(v) = raw.pointer(pointer) {
                entry
                    .1
                    .push(serde_json::to_string(v).expect("Value serializes"));
            }
        }
    }
    by_var
        .into_iter()
        .map(|(variable_id, (mut sites, mut observed))| {
            sites.sort();
            observed.sort();
            observed.dedup();
            VarUsage {
                variable_id,
                sites,
                observed,
            }
        })
        .collect()
}

/// The raw-JSON pointer prefix a style type's definition lives under on a
/// consumer node, shared by [`style_value_from_consumer`] (the definition
/// itself) and `query::styles`'s `resolve_vars` path (which variable
/// bindings belong to that definition — a binding's own pointer, e.g.
/// `/fills/0/color`, starts with this prefix).
pub fn style_value_pointer(style_type: &str) -> Option<&'static str> {
    match style_type {
        "TEXT" => Some("/style"),
        "FILL" => Some("/fills"),
        "EFFECT" => Some("/effects"),
        "GRID" => Some("/layoutGrids"),
        _ => None,
    }
}

/// Derive a style's definition from one consumer node's raw JSON.
/// Style definitions are not in the file JSON; consumers carry the
/// resolved properties.
pub fn style_value_from_consumer(style_type: &str, consumer_raw: &str) -> Option<Value> {
    let raw: Value = serde_json::from_str(consumer_raw).ok()?;
    let pointer = style_value_pointer(style_type)?;
    raw.pointer(pointer).cloned()
}
