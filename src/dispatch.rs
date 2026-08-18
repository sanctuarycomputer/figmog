//! The 17 read-only `figmog_*` tools: one dispatch function, generic over
//! the store's reader types, shared by `figmog serve`'s request loop and
//! the CLI's `figmog call`/`figmog tools`. (`figmog_sync` is not here — it
//! writes to the store and drives watch/backoff state that only exists at
//! each call site's `open_store!` — see the identical note in `serve.rs`
//! and `cli::dispatch` about the pipeline's unnameable type.)
//!
//! Both call sites destructure the same `st.rtx(|tuple| ...)` reader tuple
//! ([`RootReaders`]) that `open_store!`'s pipeline produces, so
//! [`dispatch_read_tool`] can take it directly and stay the single place
//! that maps a tool name + arguments to a `query::*` call.

use fold::pipeline::terminal::search::Bm25Reader;
use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;
use serde_json::{Value, json};

use crate::mcp::ToolDef;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, NodeRec, ProxyCacheRec, StyleRec,
    VariableCollectionRec, VariableRec,
};
use crate::query;

/// Read handles for the pipeline's `nodes` branch (see `figmog_pipeline!`
/// in `store.rs`): table, children edges, BM25 text index, then the three
/// inverted indexes plus `by_type`, in the branch's own nesting order.
pub(crate) type NodeReaders<'a, R> = (
    TableReader<'a, R, String, NodeRec>,
    MultimapReader<'a, R, String, (u32, String)>,
    Bm25Reader<'a, R, String, fn(&str, &mut Vec<u8>)>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
    InvertedIndexReader<'a, R, String, String>,
);

/// Read handles for the whole pipeline, in `figmog_pipeline!`'s top-level
/// order — the exact tuple `st.rtx`'s closure receives. 8 elements: the
/// `nodes` branch bundle, then `components`, `component_sets`, `styles`,
/// `variables`, `variable_collections`, `meta`, `proxy_cache`.
pub(crate) type RootReaders<'a, R> = (
    NodeReaders<'a, R>,
    TableReader<'a, R, String, ComponentRec>,
    TableReader<'a, R, String, ComponentSetRec>,
    TableReader<'a, R, String, StyleRec>,
    TableReader<'a, R, String, VariableRec>,
    TableReader<'a, R, String, VariableCollectionRec>,
    TableReader<'a, R, u8, FileMeta>,
    TableReader<'a, R, String, ProxyCacheRec>,
);

// ---- arg extraction ----

pub(crate) fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

/// JSON type name for an error message — `Value`'s own variant names
/// ("Number", "String", ...) aren't what a caller expects to read, so this
/// spells out the JSON vocabulary instead (`"number"`, `"string"`, ...).
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A required field that's present but the wrong JSON type is a different,
/// more actionable error than an absent one — `"expected string for `id`,
/// got number"` tells the caller what to fix; `"missing required field"`
/// would send them looking for a key that's actually right there.
pub(crate) fn require_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        None => Err(format!("missing required field: {key}")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!(
            "expected string for `{key}`, got {}",
            json_type_name(other)
        )),
    }
}

pub(crate) fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

pub(crate) fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// See [`require_str`]'s doc comment — same present-but-wrong-type
/// distinction, for numeric fields.
pub(crate) fn require_f64(args: &Value, key: &str) -> Result<f64, String> {
    match args.get(key) {
        None => Err(format!("missing required field: {key}")),
        Some(v) => v
            .as_f64()
            .ok_or_else(|| format!("expected number for `{key}`, got {}", json_type_name(v))),
    }
}

/// The file key named by a full Figma URL in whichever node-id-shaped
/// argument this call carries (spec §2b): tries `id`, then `under`, then
/// `target`, in that order — a given tool call has at most one of these,
/// so trying all three is safe. `None` when no such argument is present,
/// or it's present but isn't a URL naming a file (a bare id, or a URL
/// with no file segment). Used by `figmog serve`'s multi-file session
/// routing (`serve.rs::handle_tool_call`) to auto-infer the target mirror
/// from a pasted node/frame URL when the call omits an explicit `file`.
pub(crate) fn infer_file_from_node_ref(args: &Value) -> Option<String> {
    ["id", "under", "target"].iter().find_map(|key| {
        let raw = arg_str(args, key)?;
        crate::ident::parse_node_ref(&raw)?.0
    })
}

/// Dispatch one of the 17 read-only `figmog_*` tools against an open
/// snapshot. `upstream_status` is spliced into `figmog_status`'s output
/// (`"connected"` / `"unreachable"` / `"disabled"`) without changing
/// `query::status`'s own signature (spec §12 point 4).
///
/// Returns `None` for any name this function doesn't recognize — `figmog_*`
/// names it doesn't know (a genuine bug, since the registry gates
/// `tools/call` before this is reached) as well as `figmog_sync` and every
/// non-local name, both of which the caller handles itself.
pub(crate) fn dispatch_read_tool<R: Readable>(
    name: &str,
    args: &Value,
    upstream_status: &str,
    r: RootReaders<'_, R>,
) -> Option<Result<Value, String>> {
    let (
        (nodes, children, text, instances_of, styled_by, bound_to, by_type),
        components,
        component_sets,
        styles,
        variables,
        variable_collections,
        meta,
        _cache,
    ) = r;

    match name {
        "figmog_status" => Some(query::status(&nodes, &meta).map(|mut v| {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("upstream".to_string(), json!(upstream_status));
            }
            v
        })),
        "figmog_pages" => Some(query::pages(&nodes, &by_type)),
        "figmog_tree" => {
            let id = arg_str(args, "id");
            let depth = arg_usize(args, "depth");
            Some(query::tree(&nodes, &children, &by_type, id, depth))
        }
        "figmog_node" => Some((|| {
            let id = require_str(args, "id")?;
            let with_children = arg_bool(args, "children");
            let resolve_vars = arg_bool(args, "resolve_vars");
            query::node(
                &nodes,
                &children,
                &variables,
                &variable_collections,
                id,
                with_children,
                resolve_vars,
            )
        })()),
        "figmog_subtree" => Some((|| {
            let id = require_str(args, "id")?;
            let depth = arg_usize(args, "depth");
            let fields = args.get("fields").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
            let resolve_vars = arg_bool(args, "resolve_vars");
            query::subtree(
                &nodes,
                &children,
                &variables,
                &variable_collections,
                id,
                depth,
                fields.as_deref(),
                resolve_vars,
            )
        })()),
        "figmog_find" => Some((|| {
            let node_type = require_str(args, "type")?;
            let page = arg_str(args, "page");
            let under = arg_str(args, "under");
            query::find(&nodes, &children, &by_type, node_type, page, under)
        })()),
        "figmog_search" => Some((|| {
            let q = require_str(args, "query")?;
            let limit = arg_usize(args, "limit").unwrap_or(10);
            let under = arg_str(args, "under");
            query::search(&text, &nodes, &children, &q, limit, under)
        })()),
        "figmog_instances" => Some((|| {
            let target = require_str(args, "target")?;
            query::instances(&nodes, &components, &component_sets, &instances_of, &target)
        })()),
        "figmog_components" => Some(query::components(&nodes, &components, &component_sets)),
        "figmog_styles" => {
            let style_type = arg_str(args, "type");
            let values = arg_bool(args, "values");
            let resolve_vars = arg_bool(args, "resolve_vars");
            Some(query::styles(
                &nodes,
                &styles,
                &styled_by,
                &variables,
                &variable_collections,
                style_type,
                values,
                resolve_vars,
            ))
        }
        "figmog_uses" => Some((|| {
            let id = require_str(args, "id")?;
            query::uses(&nodes, &styled_by, &bound_to, &id)
        })()),
        "figmog_vars" => {
            let id = arg_str(args, "id");
            Some(query::vars(&nodes, &variables, &variable_collections, id))
        }
        "figmog_stats" => Some(query::stats(
            &nodes,
            &components,
            &component_sets,
            &styles,
            &variables,
            &by_type,
        )),
        "figmog_path" => Some((|| {
            let id = require_str(args, "id")?;
            query::path(&nodes, id)
        })()),
        "figmog_text" => {
            let page = arg_str(args, "page");
            let under = arg_str(args, "under");
            Some(query::text(&nodes, &children, &by_type, page, under))
        }
        "figmog_where" => Some((|| {
            let pointer = require_str(args, "pointer")?;
            let equals = args.get("equals").cloned();
            let page = arg_str(args, "page");
            let under = arg_str(args, "under");
            query::where_(&nodes, &children, &pointer, equals, page, under)
        })()),
        "figmog_at" => Some((|| {
            let x = require_f64(args, "x")?;
            let y = require_f64(args, "y")?;
            query::at(&nodes, x, y)
        })()),
        _ => None,
    }
}

/// The optional `file` property every local tool's schema carries as of
/// v4 (spec §14): a Figma file URL or key, routed by `SessionManager`
/// (`sessions.rs`) to the mirror it names, auto-opening it (one Tier-1
/// pull) if it's new. Omitted, a tool targets the default mirrored file.
fn file_arg_property() -> Value {
    json!({
        "type": "string",
        "description": "Figma file URL or key; omit for the default mirrored file."
    })
}

/// The 20 `figmog_*` MCP tools (spec §14, v4; `figmog_subtree` added by
/// v0.0.2 §2): 17 reads of the local mirror (`figmog_subtree` among them)
/// plus `figmog_sync` — 18 total — every one of which gains the optional
/// `file` routing property below — plus the two v4 additions, `figmog_open`
/// and `figmog_files`, which don't (routing *to* a file, and listing every
/// file, aren't themselves per-file operations). Every tool but
/// `figmog_sync`/`figmog_open` reads the local mirror at zero Figma API
/// cost.
pub(crate) fn tool_registry() -> Vec<ToolDef> {
    let mut tools = vec![
        ToolDef {
            name: "figmog_status",
            description: "File name, version, last modified time, and node count — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_pages",
            description: "List the file's pages (CANVAS nodes), in document order — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_tree",
            description: "Subtree outline (id, name, type, children) rooted at a node, defaulting to the whole document — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Root node id; defaults to the DOCUMENT node."},
                    "depth": {"type": "integer", "description": "Max depth to descend; omitted means unlimited."}
                }
            }),
        },
        ToolDef {
            name: "figmog_node",
            description: "Full raw JSON of one node by id, optionally with a one-level children summary and/or a resolved_variables annotation — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Node id (12:34 or 12-34 form)."},
                    "children": {"type": "boolean", "description": "Inline a one-level children summary."},
                    "resolve_vars": {"type": "boolean", "description": "Annotate boundVariables binding sites with variable names/values under a resolved_variables key."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_subtree",
            description: "Full raw JSON of a node and its descendants, nested under `children` in child-index order — reads the local mirror (no Figma API cost). Use `depth` and `fields` to keep the response small; an unbounded dump of a large subtree can be huge.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Root node id (12:34 or 12-34 form)."},
                    "depth": {"type": "integer", "description": "Max depth to descend; omitted means unlimited."},
                    "fields": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Project every node to these raw fields (id/name/type/children always survive). Recommended for large subtrees."
                    },
                    "resolve_vars": {"type": "boolean", "description": "Annotate boundVariables binding sites with variable names/values under a resolved_variables key."}
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_find",
            description: "Nodes by Figma node type, optionally scoped to one page and/or a subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "Figma node type, e.g. FRAME."},
                    "page": {"type": "string", "description": "Page (CANVAS) node id to scope to."},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["type"]
            }),
        },
        ToolDef {
            name: "figmog_search",
            description: "BM25 search over layer names and text content, optionally scoped to a subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "description": "Max hits (default 10)."},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "figmog_instances",
            description: "Instances of a component, resolved by node id, global key, or component/component-set name — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Node id, key, or name of a component or component set."}
                },
                "required": ["target"]
            }),
        },
        ToolDef {
            name: "figmog_components",
            description: "Design-system inventory: component sets with their variant axes, plus standalone components — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_styles",
            description: "Styles with usage counts; `values` derives each style's definition from a consumer node — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "type": {"type": "string", "description": "Style type filter, e.g. FILL, TEXT."},
                    "values": {"type": "boolean", "description": "Derive each style's definition from a consumer node."},
                    "resolve_vars": {"type": "boolean", "description": "With values, annotate the definition's variable bindings under a resolved_variables key."}
                }
            }),
        },
        ToolDef {
            name: "figmog_uses",
            description: "Nodes using a style id or bound to a variable id — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "A style id or variable id."}},
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_vars",
            description: "Variables: the authoritative record if imported via figmog import-variables, else inferred from bindings — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Variable id filter; omitted means all variables."}}
            }),
        },
        ToolDef {
            name: "figmog_sync",
            description: "Forces one pull from Figma and returns the sync churn (+added ~changed -removed) — fetches from Figma (spends Tier-1 rate budget).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_stats",
            description: "Node counts by type and by page, component/set/style/variable totals, text-node count, max tree depth — reads the local mirror (no Figma API cost).",
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolDef {
            name: "figmog_path",
            description: "Ancestor chain from the document root to a node, as [{id, name, type}] — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "figmog_text",
            description: "Every TEXT node's (id, characters, page_id), optionally scoped to one page and/or a subtree, sorted by id — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page": {"type": "string"},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                }
            }),
        },
        ToolDef {
            name: "figmog_where",
            description: "Nodes whose raw JSON matches an RFC 6901 pointer, optionally filtered by value and/or scoped to a page/subtree — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pointer": {"type": "string", "description": "RFC 6901 pointer into the node's raw JSON, e.g. /layoutMode."},
                    "equals": {"description": "JSON value to match; omitted means \"pointer exists\"."},
                    "page": {"type": "string"},
                    "under": {"type": "string", "description": "Scope to the subtree rooted at this node id (inclusive)."}
                },
                "required": ["pointer"]
            }),
        },
        ToolDef {
            name: "figmog_at",
            description: "Nodes whose absolute bounds contain a point, sorted by area ascending (deepest/smallest first) — reads the local mirror (no Figma API cost).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"},
                    "y": {"type": "number"}
                },
                "required": ["x", "y"]
            }),
        },
    ];

    for t in tools.iter_mut() {
        if let Some(props) = t
            .input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            props.insert("file".to_string(), file_arg_property());
        }
    }

    tools.push(ToolDef {
        name: "figmog_open",
        description: "Mirror a Figma file now (spends one Tier-1 pull) — creates the mirror if it's new, or re-syncs it if already mirrored. Returns the sync churn and node count.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "file": {"type": "string", "description": "Figma file URL or key to mirror."}
            },
            "required": ["file"]
        }),
    });
    tools.push(ToolDef {
        name: "figmog_files",
        description: "List every mirrored file: key, name, version, node count, last synced time, and which one is the default — reads local state only (no Figma API cost).",
        input_schema: json!({"type": "object", "properties": {}}),
    });

    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_str_reports_wrong_type_not_missing() {
        let err = require_str(&json!({"id": 12}), "id").unwrap_err();
        assert_eq!(err, "expected string for `id`, got number");
    }

    #[test]
    fn require_str_reports_missing_when_key_absent() {
        let err = require_str(&json!({}), "id").unwrap_err();
        assert_eq!(err, "missing required field: id");
    }

    #[test]
    fn require_f64_reports_wrong_type_not_missing() {
        let err = require_f64(&json!({"x": "12"}), "x").unwrap_err();
        assert_eq!(err, "expected number for `x`, got string");
    }

    #[test]
    fn require_f64_reports_missing_when_key_absent() {
        let err = require_f64(&json!({}), "x").unwrap_err();
        assert_eq!(err, "missing required field: x");
    }
}
