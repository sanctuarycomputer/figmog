//! Record vocabulary shared by flatten, store, and the CLI.
//!
//! Determinism contract: every map-shaped field is a **sorted** `Vec` of
//! pairs, and canonical-JSON strings come from `serde_json` without
//! `preserve_order`. `KeyedStream` diffs records by postcard bytes, so two
//! flattens of the same file JSON must be byte-identical.

use serde::{Deserialize, Serialize};

/// Primary key of every mirrored record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Id {
    Node(String),
    Component(String),
    ComponentSet(String),
    Style(String),
    Variable(String),
    VariableCollection(String),
    Meta,
    /// Cached upstream proxy response, keyed by its hash (spec §12). APPEND
    /// ONLY: postcard encodes variant indices, so inserting a variant
    /// earlier in this enum would corrupt every existing store.
    ProxyCache(String),
}

/// One mirrored record; variant always matches its [`Id`] variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Rec {
    Node(NodeRec),
    Component(ComponentRec),
    ComponentSet(ComponentSetRec),
    Style(StyleRec),
    Variable(VariableRec),
    VariableCollection(VariableCollectionRec),
    Meta(FileMeta),
    /// See [`Id::ProxyCache`]. APPEND ONLY — see that variant's note.
    ProxyCache(ProxyCacheRec),
}

/// One node of the document tree (children stripped from `raw`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRec {
    pub id: String,
    pub parent_id: Option<String>,
    pub child_index: u32,
    /// Enclosing CANVAS id; the document root and CANVAS nodes carry their own id.
    pub page_id: String,
    pub node_type: String,
    pub name: String,
    pub visible: bool,
    /// `characters` for TEXT nodes.
    pub text: Option<String>,
    /// INSTANCE → the component's node id.
    pub component_id: Option<String>,
    /// INSTANCE `componentProperties` as (name, canonical-JSON value), sorted.
    pub component_properties: Vec<(String, String)>,
    /// `componentPropertyDefinitions` (COMPONENT / COMPONENT_SET) as canonical JSON.
    pub property_definitions: Option<String>,
    /// Node `styles` map as (style_type, style_id), sorted.
    pub style_refs: Vec<(String, String)>,
    /// Variable bindings as (json-pointer to the bound property, variable id), sorted.
    /// The pointer addresses the *resolved value* location, e.g. `/fills/0/color`.
    pub bound_variables: Vec<(String, String)>,
    /// absoluteBoundingBox x, y, w, h.
    pub abs_bounds: Option<[f64; 4]>,
    /// Canonical JSON of the node with `children` removed.
    pub raw: String,
}

/// Entry of the file response's `components` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentRec {
    pub node_id: String,
    pub key: String,
    pub name: String,
    pub description: String,
    pub component_set_id: Option<String>,
    pub remote: bool,
}

/// Entry of the file response's `componentSets` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSetRec {
    pub node_id: String,
    pub key: String,
    pub name: String,
    pub description: String,
    pub remote: bool,
}

/// Entry of the file response's `styles` map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StyleRec {
    pub style_id: String,
    pub key: String,
    pub name: String,
    pub style_type: String,
    pub description: String,
    pub remote: bool,
}

/// Authoritative variable definition (from `import-variables` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableRec {
    pub id: String,
    pub name: String,
    pub resolved_type: String,
    pub collection_id: String,
    /// mode id -> value-or-alias, as sorted (mode_id, canonical JSON) pairs.
    pub values_by_mode: Vec<(String, String)>,
    pub description: String,
    pub scopes: Vec<String>,
}

/// Authoritative variable collection (from `import-variables` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableCollectionRec {
    pub id: String,
    pub name: String,
    /// (mode_id, mode_name), sorted by mode_id.
    pub modes: Vec<(String, String)>,
    pub default_mode_id: String,
}

/// The single file-level row (key [`Id::Meta`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub name: String,
    pub version: String,
    pub last_modified: String,
    pub synced_at_unix_ms: u64,
}

/// One cached upstream proxy response (spec §12). A hit requires
/// `file_version` to equal the mirror's current [`FileMeta::version`]; a
/// version bump makes the row stale and eligible for eviction
/// (`store::stale_cache_ids` / `store::evict_stale_cache`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyCacheRec {
    /// Hash of `tool` + `args_canonical`; identical to the [`Id::ProxyCache`] key.
    pub key_hash: String,
    pub tool: String,
    /// Canonical JSON (`serde_json::to_string`) of the call arguments.
    pub args_canonical: String,
    /// File version this response was fetched at.
    pub file_version: String,
    /// Canonical JSON of the upstream MCP result content.
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node() -> NodeRec {
        NodeRec {
            id: "1:2".into(),
            parent_id: Some("0:1".into()),
            child_index: 0,
            page_id: "0:1".into(),
            node_type: "TEXT".into(),
            name: "Title".into(),
            visible: true,
            text: Some("hello".into()),
            component_id: None,
            component_properties: vec![("Size".into(), "\"Large\"".into())],
            property_definitions: None,
            style_refs: vec![("text".into(), "S:2".into())],
            bound_variables: vec![("/style/fontSize".into(), "VariableID:9".into())],
            abs_bounds: Some([0.0, 0.0, 100.0, 20.0]),
            raw: "{}".into(),
        }
    }

    #[test]
    fn rec_postcard_roundtrip() {
        let rec = Rec::Node(sample_node());
        let bytes = postcard::to_allocvec(&rec).unwrap();
        let back: Rec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn identical_records_encode_identically() {
        let a = postcard::to_allocvec(&Rec::Node(sample_node())).unwrap();
        let b = postcard::to_allocvec(&Rec::Node(sample_node())).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn ids_order_and_roundtrip() {
        let ids = vec![Id::Meta, Id::Node("1:1".into()), Id::Style("S:1".into())];
        let set: std::collections::BTreeSet<Id> = ids.iter().cloned().collect();
        assert_eq!(set.len(), 3);
        let bytes = postcard::to_allocvec(&ids).unwrap();
        let back: Vec<Id> = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(ids, back);
    }
}
