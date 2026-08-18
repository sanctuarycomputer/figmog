//! Cached-proxy routing rules (build design §12): the namespace rule,
//! registry merge, and the cacheable-call rule. Pure and side-effect-free
//! (no store, no upstream I/O) so they're unit-testable in isolation —
//! `serve.rs` and `cli/` both drive real store/upstream state through
//! these same decisions.

use serde_json::{Value, json};

use fold::pipeline::{Keyed, Push};
use fold::stream::KeyedStream;

use crate::cache;
use crate::mcp::ToolDef;
use crate::model::{Id, Rec};
use crate::upstream::UpstreamMcp;

/// The namespace rule (spec §12, §11 point 3): `figmog_*` tools are always
/// local; every other name is proxied to the upstream (when attached).
pub(crate) fn is_local_tool(name: &str) -> bool {
    name.starts_with("figmog_")
}

/// Name-only half of the cacheable rule: whether this tool's *kind* is
/// cacheable in principle. A concrete call is only actually cached when it
/// also carries [`has_explicit_node_id`] — this half alone is what `figmog
/// tools`'s `cacheable` column reports, since a tool listing has no call
/// arguments to inspect.
pub(crate) fn tool_name_cache_capable(name: &str) -> bool {
    name.starts_with("get_") || name.starts_with("list_")
}

/// Whether `args` carries an explicit node id under any of the key names
/// Figma's native tools use for one (`nodeId`, `node_id`, `id`), as a
/// *string* value — selection-based calls (no such key, or a non-string
/// value) are invisible to the cache and always forwarded.
pub(crate) fn has_explicit_node_id(args: &Value) -> bool {
    ["nodeId", "node_id", "id"]
        .iter()
        .any(|key| matches!(args.get(*key), Some(Value::String(_))))
}

/// The full cacheable rule (spec §12 "Cache"): `get_*`/`list_*` AND an
/// explicit node id in the call arguments.
pub(crate) fn is_cacheable(name: &str, args: &Value) -> bool {
    tool_name_cache_capable(name) && has_explicit_node_id(args)
}

/// Canonical JSON of a call's arguments, for the cache key and stored row.
/// `serde_json::Value` objects are backed by a `BTreeMap` (this crate never
/// enables the `preserve_order` feature), so `to_string` already yields
/// sorted-key, deterministic output — no extra normalization needed.
/// Propagates a serialize failure rather than collapsing it to `""`: two
/// *different* calls that both failed to serialize would otherwise share
/// one cache key/lookup (`""`), letting one tool's cached response answer
/// another's request.
pub(crate) fn canonical_args(args: &Value) -> Result<String, String> {
    serde_json::to_string(args).map_err(|e| e.to_string())
}

/// `tools/list` = the local `figmog_*` registry followed by every upstream
/// tool verbatim (name/inputSchema passed through, description prefixed
/// `"[via Figma desktop] "`). Returns the merged list plus the names of any
/// upstream tools dropped for colliding with the local registry OR with an
/// upstream tool already accepted earlier in this same call — either the
/// `figmog_*` namespace (the namespace rule makes this impossible for
/// figmog's own registry, but a live desktop server's tool list is outside
/// figmog's control), a defensive exact match against a current local
/// tool's name even without the prefix, or two upstream entries sharing a
/// name (a malformed/duplicated upstream tool list must not produce two
/// `ToolDef`s of the same name in the merged registry) — the caller logs
/// the drops.
pub(crate) fn merge_registry(
    mut local: Vec<ToolDef>,
    upstream_tools: &[Value],
) -> (Vec<ToolDef>, Vec<String>) {
    let mut seen_names: std::collections::BTreeSet<&str> = local.iter().map(|t| t.name).collect();
    let mut dropped = Vec::new();
    for tool in upstream_tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if is_local_tool(name) || seen_names.contains(name) {
            dropped.push(name.to_string());
            continue;
        }
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        // Upstream tool names/descriptions are only known once, at startup
        // (no mid-session re-probe in v3 — spec §12), and figmog serves for
        // the life of the process: leaking these few dozen strings to get
        // the `&'static str` `ToolDef` needs is bounded and never repeats.
        let leaked_name: &'static str = Box::leak(name.to_string().into_boxed_str());
        seen_names.insert(leaked_name);
        local.push(ToolDef {
            name: leaked_name,
            description: Box::leak(format!("[via Figma desktop] {description}").into_boxed_str()),
            input_schema,
        });
    }
    (local, dropped)
}

/// Execute one proxied `tools/call`. `version_and_hit` is `(current
/// `FileMeta.version`, cache hit if any)`, already read via `st.rtx` at the
/// call site — the reader tuple's shape is pinned to one concrete
/// `open_store!` call site (see `dispatch.rs`'s doc comment), so this
/// function, generic only over `P: Push<..>`, can't read the store itself;
/// it only writes to it (`cache::store`, via `wtx`, has no such
/// restriction).
///
/// Returns the value to hand back to the MCP client, and whether the
/// caller should trigger an immediate meta-poll tick (spec §12 "Writes": a
/// successful call to a tool that isn't `get_*`/`list_*` may have changed
/// the file).
pub(crate) fn proxy_call<U: UpstreamMcp, P: Push<Keyed<Id, Rec>>>(
    st: &mut KeyedStream<Id, Rec, P>,
    upstream: &mut U,
    name: &str,
    args: &Value,
    version_and_hit: (Option<String>, Option<Value>),
) -> Result<(Value, bool), String> {
    let (version, hit) = version_and_hit;
    if let Some(hit) = hit {
        return Ok((hit, false));
    }

    let result = upstream.call(name, args).map_err(|e| e.to_string())?;

    if is_cacheable(name, args) {
        // Never cache a tool-level failure (spec §12's cache is a response
        // cache, not an error cache): an upstream `isError: true` result
        // still passes through to the client verbatim (its own `isError`
        // preserved), it just isn't written to `proxy_cache`, so the next
        // identical call gets a fresh attempt instead of a stuck failure.
        let is_error = result.get("isError") == Some(&Value::Bool(true));
        if !is_error && let Some(version) = &version {
            // A failure here (`canonical_args`'/`cache::store`'s only real
            // failure mode: `content` can't be serialized — see
            // `cache::store`'s doc comment) is a caching problem, not an
            // upstream one: the tool call itself already succeeded, and a
            // successful response reaching the client must not be turned
            // into a call failure just because writing it to `proxy_cache`
            // didn't work. Log and move on; the next identical call simply
            // misses the cache and re-fetches, same as today.
            match canonical_args(args).and_then(|args_canonical| {
                cache::store(st, name, &args_canonical, version, &result)
            }) {
                Ok(()) => {}
                Err(e) => eprintln!("figmog: failed to cache {name} response: {e}"),
            }
        }
        return Ok((result, false));
    }

    let trigger_poll = !tool_name_cache_capable(name);
    Ok((result, trigger_poll))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{FakeUpstream, UpstreamError};

    fn local_tool(name: &'static str) -> ToolDef {
        ToolDef {
            name,
            description: "d",
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn is_local_tool_matches_only_figmog_prefix() {
        assert!(is_local_tool("figmog_status"));
        assert!(!is_local_tool("get_code"));
        assert!(!is_local_tool("list_something"));
    }

    #[test]
    fn cacheable_requires_get_or_list_prefix_and_string_node_id() {
        assert!(is_cacheable("get_code", &json!({"nodeId": "1:2"})));
        assert!(is_cacheable("list_variables", &json!({"id": "1:2"})));
        assert!(is_cacheable("get_code", &json!({"node_id": "1:2"})));
        // Not get_/list_.
        assert!(!is_cacheable(
            "add_code_connect_map",
            &json!({"nodeId": "1:2"})
        ));
        // No node id at all: selection-based call.
        assert!(!is_cacheable("get_code", &json!({})));
        // Node id present but not a string.
        assert!(!is_cacheable("get_code", &json!({"nodeId": 12})));
    }

    #[test]
    fn canonical_args_is_stable_regardless_of_source_key_order() {
        let a: Value = serde_json::from_str(r#"{"nodeId":"1:2","depth":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"depth":1,"nodeId":"1:2"}"#).unwrap();
        assert_eq!(canonical_args(&a), canonical_args(&b));
    }

    #[test]
    fn merge_registry_appends_upstream_tools_with_prefixed_description() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![json!({
            "name": "get_code",
            "description": "Returns code for a node",
            "inputSchema": {"type": "object", "properties": {"nodeId": {"type": "string"}}},
        })];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert!(dropped.is_empty());
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "figmog_status");
        assert_eq!(merged[1].name, "get_code");
        assert_eq!(
            merged[1].description,
            "[via Figma desktop] Returns code for a node"
        );
        assert_eq!(
            merged[1].input_schema,
            json!({"type": "object", "properties": {"nodeId": {"type": "string"}}})
        );
    }

    #[test]
    fn merge_registry_drops_and_reports_upstream_tool_named_like_local() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![
            json!({"name": "figmog_evil", "description": "impersonating"}),
            json!({"name": "get_code", "description": "fine"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["figmog_evil".to_string()]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|t| t.name == "get_code"));
        assert!(!merged.iter().any(|t| t.name == "figmog_evil"));
    }

    #[test]
    fn merge_registry_drops_upstream_tool_matching_local_name_without_prefix() {
        // A local tool name without the `figmog_` prefix (hypothetical, but
        // the dedup must key off actual local names, not just the prefix
        // rule) still shadows an identically-named upstream tool.
        let local = vec![local_tool("status")];
        let upstream = vec![
            json!({"name": "status", "description": "impersonating"}),
            json!({"name": "get_code", "description": "fine"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["status".to_string()]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|t| t.name == "get_code"));
        assert_eq!(merged.iter().filter(|t| t.name == "status").count(), 1);
    }

    #[test]
    fn merge_registry_defaults_missing_description_and_schema() {
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![json!({"name": "get_screenshot"})];
        let (merged, _dropped) = merge_registry(local, &upstream);
        assert_eq!(merged[1].description, "[via Figma desktop] ");
        assert_eq!(merged[1].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn merge_registry_drops_a_second_upstream_tool_with_a_duplicate_name() {
        // Two upstream entries sharing a name (a malformed/duplicated
        // upstream tool list) must still only produce one `ToolDef` in the
        // merged registry — the second is dropped, not appended as a
        // duplicate.
        let local = vec![local_tool("figmog_status")];
        let upstream = vec![
            json!({"name": "get_code", "description": "first"}),
            json!({"name": "get_code", "description": "second"}),
        ];
        let (merged, dropped) = merge_registry(local, &upstream);
        assert_eq!(dropped, vec!["get_code".to_string()]);
        assert_eq!(merged.iter().filter(|t| t.name == "get_code").count(), 1);
        assert_eq!(
            merged
                .iter()
                .find(|t| t.name == "get_code")
                .unwrap()
                .description,
            "[via Figma desktop] first"
        );
    }

    // ---- proxy_call routing ----

    #[test]
    fn proxy_call_cache_hit_never_calls_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        // No result queued: if `call` were invoked, the fake would return a
        // "no scripted result queued" error, so a returned `Ok` proves the
        // hit short-circuited the upstream entirely.
        let (value, poll) = proxy_call(
            &mut st,
            &mut upstream,
            "get_code",
            &json!({"nodeId": "1:2"}),
            (Some("100".to_string()), Some(json!({"cached": true}))),
        )
        .unwrap();
        assert_eq!(value, json!({"cached": true}));
        assert!(!poll);
        assert_eq!(upstream.call_count, 0);
    }

    #[test]
    fn proxy_call_miss_calls_upstream_and_stores_cache_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Ok(json!({"content": [{"type": "text", "text": "fresh"}]})));

        let (value, poll) = proxy_call(
            &mut st,
            &mut upstream,
            "get_code",
            &json!({"nodeId": "1:2"}),
            (Some("100".to_string()), None),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"content": [{"type": "text", "text": "fresh"}]})
        );
        assert!(!poll);
        assert_eq!(upstream.call_count, 1);

        let stored = st.rtx(|(_, _, _, _, _, _, _, cache, _, _)| {
            cache::lookup(
                &cache,
                "get_code",
                &canonical_args(&json!({"nodeId": "1:2"})).unwrap(),
                "100",
            )
        });
        assert_eq!(stored, Some(value));
    }

    /// M3 (spec §4 debt ledger): a cache-write failure must never turn a
    /// successful upstream call into a failed one — `proxy_call` logs to
    /// stderr and still hands the client the successful result. `content: &
    /// serde_json::Value` can't actually construct a non-finite float
    /// through the crate's public API (`Number::from_f64`/`From<f64>` both
    /// reject NaN/infinity outright), so `cache::store`'s only documented
    /// failure mode is unreachable from safe code — this test instead pins
    /// the happy path this refactor must leave unchanged (value + poll
    /// flag + the row landing in `proxy_cache`), the same "proven by
    /// inspection, not by triggering it" stance `open_store_checked`'s own
    /// doc comment takes for its equally unreachable non-lock-panic branch.
    #[test]
    fn proxy_call_cache_write_never_fails_the_call_on_the_happy_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Ok(json!({"content": [{"type": "text", "text": "fresh"}]})));

        let result = proxy_call(
            &mut st,
            &mut upstream,
            "get_code",
            &json!({"nodeId": "1:2"}),
            (Some("100".to_string()), None),
        );
        assert!(result.is_ok(), "a successful upstream call must stay Ok");
        let (value, poll) = result.unwrap();
        assert_eq!(
            value,
            json!({"content": [{"type": "text", "text": "fresh"}]})
        );
        assert!(!poll);
    }

    #[test]
    fn proxy_call_non_cacheable_write_triggers_meta_poll() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Ok(json!({"ok": true})));

        let (_value, poll) = proxy_call(
            &mut st,
            &mut upstream,
            "add_code_connect_map",
            &json!({}),
            (None, None),
        )
        .unwrap();
        assert!(poll, "a non-get/list write should trigger a meta poll");
    }

    #[test]
    fn proxy_call_get_prefixed_without_node_id_forwards_uncached_and_no_poll() {
        // Selection-based `get_*` calls are invisible to the cache (spec
        // §12), but they're still reads, so they must not trigger a poll.
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Ok(json!({"selection": true})));

        let (value, poll) = proxy_call(
            &mut st,
            &mut upstream,
            "get_code",
            &json!({}),
            (Some("100".to_string()), None),
        )
        .unwrap();
        assert_eq!(value, json!({"selection": true}));
        assert!(!poll);
        assert_eq!(upstream.call_count, 1);
    }

    #[test]
    fn proxy_call_cacheable_tool_error_is_forwarded_but_not_cached() {
        // An upstream tool-level failure (isError: true in a successful
        // Ok(...) result — not an UpstreamError) must reach the client
        // verbatim, but must NOT be written to the cache: otherwise a
        // transient failure would be replayed forever on every later call
        // for the same node.
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        let error_result = json!({
            "content": [{"type": "text", "text": "node not found"}],
            "isError": true,
        });
        upstream.push_result(Ok(error_result.clone()));

        let (value, poll) = proxy_call(
            &mut st,
            &mut upstream,
            "get_code",
            &json!({"nodeId": "1:2"}),
            (Some("100".to_string()), None),
        )
        .unwrap();
        assert_eq!(value, error_result);
        assert!(!poll);

        let stored = st.rtx(|(_, _, _, _, _, _, _, cache, _, _)| {
            cache::lookup(
                &cache,
                "get_code",
                &canonical_args(&json!({"nodeId": "1:2"})).unwrap(),
                "100",
            )
        });
        assert_eq!(stored, None, "an isError result must never be cached");
    }

    #[test]
    fn proxy_call_propagates_upstream_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = crate::open_store!(dir.path().join("db"));
        let mut upstream = FakeUpstream::new(vec![]);
        upstream.push_result(Err(UpstreamError::Protocol("boom".into())));

        let err =
            proxy_call(&mut st, &mut upstream, "get_code", &json!({}), (None, None)).unwrap_err();
        assert!(err.contains("boom"));
    }
}
