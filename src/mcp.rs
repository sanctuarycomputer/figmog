//! MCP protocol core — pure JSON-RPC dispatch over stdio frames.
//!
//! This module owns no I/O and no store: [`handle_message`] takes one raw
//! JSON-RPC message, a registry of [`ToolDef`]s, and a [`ToolHandler`], and
//! returns the response frame to write (or `None` for notifications). The
//! serve loop that actually reads/writes stdio lives in a later task.

use serde_json::{Value, json};

/// figmog is your Figma server: a local, instant mirror of one Figma file
/// plus a cached proxy to Figma's native capabilities. Call figmog for
/// everything Figma-related. figmog_* tools answer from the local mirror at
/// zero API cost; native-named tools (get_*, …) go to Figma, cached by file
/// version where possible.
///
/// This is the exact steering text carried verbatim in the `initialize`
/// result's `instructions` field — see build design §12 / §11 point 3
/// (v3, cached-proxy positioning: figmog is the ONLY Figma MCP an agent
/// connects to, superseding the v2 "second, separate server" text) plus,
/// as of v4 (spec §14), one appended sentence steering agents toward the
/// `file` argument and figmog's auto-mirror-on-first-reference behavior.
const INSTRUCTIONS: &str = "figmog is your Figma server: a local, instant mirror of one Figma file plus a cached proxy to Figma's native capabilities. Call figmog for everything Figma-related. figmog_* tools answer from the local mirror at zero API cost; native-named tools (get_*, …) go to Figma, cached by file version where possible. Pass the Figma file URL as the `file` argument when you have one; figmog mirrors files on first reference.";

/// The default MCP protocol version echoed when a client's `initialize`
/// request omits `protocolVersion`.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// This server's name, reported in `initialize`'s `serverInfo.name`.
pub const SERVER_NAME: &str = "figmog";

/// One registered tool: metadata for `tools/list`.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's arguments.
    pub input_schema: Value,
}

/// What a `tools/call` handler produces on success (spec §11/§12: local
/// `figmog_*` tools own their own JSON shape and answer it as MCP text
/// content the way figmog always has; proxied tools' results are already a
/// complete, correctly-shaped MCP `CallToolResult` produced by the
/// upstream — re-wrapping that as ANOTHER text block would double-encode
/// it and, for a non-text content type such as `get_screenshot`'s image
/// block, make it unrenderable).
pub enum ToolOutput {
    /// figmog's own JSON, serialized into a single text content block —
    /// today's (v1/v2) behavior, still used for every local `figmog_*`
    /// tool.
    Json(Value),
    /// A complete MCP `tools/call` result, emitted verbatim as the
    /// JSON-RPC `result` member. Used for proxied calls, whose shape (and
    /// `isError`) is the upstream's to own.
    Raw(Value),
}

/// Executes a `tools/call`. `Ok(output)` becomes the success result per
/// [`ToolOutput`]'s two shapes; `Err(msg)` becomes `isError` text content.
pub trait ToolHandler {
    fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String>;
}

/// Adapts a closure to [`ToolHandler`]. `figmog serve`'s store handle has
/// an unnameable type (the `open_store!` pipeline contains fn items), so
/// it can't be held in a named struct field generic over the store's
/// pipeline type; wrapping a closure that captures the store by unique
/// reference sidesteps that entirely — the closure's environment can hold
/// whatever concrete type it was defined against.
pub struct FnHandler<F>(pub F);

impl<F: FnMut(&str, &Value) -> Result<ToolOutput, String>> ToolHandler for FnHandler<F> {
    fn call(&mut self, name: &str, args: &Value) -> Result<ToolOutput, String> {
        (self.0)(name, args)
    }
}

/// Handle one incoming JSON-RPC message. Returns the response frame to
/// write, or `None` for notifications (a message with no `id`, or whose
/// method starts with `notifications/`).
pub fn handle_message(
    raw: &str,
    tools: &[ToolDef],
    handler: &mut dyn ToolHandler,
) -> Option<Value> {
    let msg: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": "parse error"},
            }));
        }
    };

    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications: no `id`, or method under the `notifications/` namespace.
    if id.is_none() || method.starts_with("notifications/") {
        return None;
    }
    let id = id.unwrap();

    let result = match method {
        "initialize" => Some(initialize_result(&params)),
        "ping" => Some(json!({})),
        "tools/list" => Some(tools_list_result(tools)),
        "tools/call" => Some(tools_call_result(&params, tools, handler)),
        _ => None,
    };

    match result {
        Some(result) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })),
        None => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found"},
        })),
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn tools_list_result(tools: &[ToolDef]) -> Value {
    let list: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect();
    json!({"tools": list})
}

fn tools_call_result(params: &Value, tools: &[ToolDef], handler: &mut dyn ToolHandler) -> Value {
    let name = match params.get("name").and_then(Value::as_str) {
        Some(name) => name,
        None => return error_content("missing required field: name"),
    };

    if !tools.iter().any(|t| t.name == name) {
        return error_content(&format!("unknown tool: {name}"));
    }

    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match handler.call(name, &args) {
        Ok(ToolOutput::Json(v)) => json!({
            "content": [{"type": "text", "text": serde_json::to_string(&v).unwrap()}],
            "isError": false,
        }),
        Ok(ToolOutput::Raw(v)) => v,
        Err(msg) => error_content(&msg),
    }
}

fn error_content(msg: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": msg}],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ToolHandler` test double: returns `Ok(Json({"ok":true}))` for a
    /// tool named `"ok"`, `Ok(Raw(...))` for `"raw"` (an already-complete
    /// MCP result, as a proxied call would produce), `Err("boom")` for
    /// `"err"`, and panics for any other name (the dispatch contract
    /// guarantees unknown names never reach the handler).
    struct FakeHandler;

    impl ToolHandler for FakeHandler {
        fn call(&mut self, name: &str, _args: &Value) -> Result<ToolOutput, String> {
            match name {
                "ok" => Ok(ToolOutput::Json(json!({"ok": true}))),
                "raw" => Ok(ToolOutput::Raw(json!({
                    "content": [{"type": "image", "data": "base64==", "mimeType": "image/png"}],
                    "isError": false,
                }))),
                "err" => Err("boom".to_string()),
                other => panic!("handler should not be called for {other}"),
            }
        }
    }

    fn fake_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "ok",
                description: "always succeeds",
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "raw",
                description: "returns a raw passthrough result",
                input_schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "err",
                description: "always fails",
                input_schema: json!({"type": "object"}),
            },
        ]
    }

    #[test]
    fn parse_failure_returns_dash_32700_with_null_id() {
        let resp = handle_message("not json", &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "parse error"},
            })
        );
    }

    #[test]
    fn initialize_echoes_client_protocol_version_and_carries_instructions_verbatim() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "figmog", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "figmog is your Figma server: a local, instant mirror of one Figma file plus a cached proxy to Figma's native capabilities. Call figmog for everything Figma-related. figmog_* tools answer from the local mirror at zero API cost; native-named tools (get_*, …) go to Figma, cached by file version where possible. Pass the Figma file URL as the `file` argument when you have one; figmog mirrors files on first reference.",
                },
            })
        );
    }

    #[test]
    fn initialize_defaults_protocol_version_when_absent() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {},
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-06-18"));
    }

    #[test]
    fn notifications_initialized_returns_none() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn any_notifications_namespaced_method_returns_none_even_with_id() {
        // Per the brief, method starting "notifications/" is a notification
        // regardless of whether an `id` happens to be present.
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "notifications/whatever",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn message_with_no_id_returns_none() {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": "ping",
        })
        .to_string();
        assert_eq!(handle_message(&raw, &[], &mut FakeHandler), None);
    }

    #[test]
    fn ping_returns_empty_result() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp, json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
    }

    #[test]
    fn tools_list_reflects_fake_slice_in_order() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "tools": [
                        {"name": "ok", "description": "always succeeds", "inputSchema": {"type": "object"}},
                        {"name": "raw", "description": "returns a raw passthrough result", "inputSchema": {"type": "object"}},
                        {"name": "err", "description": "always fails", "inputSchema": {"type": "object"}},
                    ],
                },
            })
        );
    }

    #[test]
    fn tools_call_success_wraps_handler_ok_as_text_content() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "ok", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "{\"ok\":true}"}],
                    "isError": false,
                },
            })
        );
    }

    #[test]
    fn tools_call_raw_output_is_emitted_verbatim_as_the_result_member() {
        // A proxied call's result is already a complete MCP `CallToolResult`
        // (e.g. an image content block for a screenshot tool) — `Raw` must
        // pass it through untouched, NOT re-wrap it in another text block
        // (which would double-encode it and make it unrenderable).
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "raw", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "image", "data": "base64==", "mimeType": "image/png"}],
                    "isError": false,
                },
            })
        );
    }

    #[test]
    fn tools_call_handler_error_becomes_is_error_content() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "err", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "boom"}],
                    "isError": true,
                },
            })
        );
    }

    #[test]
    fn tools_call_unknown_tool_is_error_without_calling_handler() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "nonexistent", "arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        // FakeHandler panics if called with an unregistered name, so a clean
        // result here proves the handler was never invoked.
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "content": [{"type": "text", "text": "unknown tool: nonexistent"}],
                    "isError": true,
                },
            })
        );
    }

    #[test]
    fn tools_call_missing_name_is_error_without_calling_handler() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"arguments": {}},
        })
        .to_string();
        let tools = fake_tools();
        let resp = handle_message(&raw, &tools, &mut FakeHandler).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
        assert_eq!(resp["result"]["content"][0]["type"], json!("text"));
        // A clear message, not a panic or empty string.
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.is_empty());
    }

    #[test]
    fn unknown_method_with_id_returns_dash_32601() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "totally/bogus",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(
            resp,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": -32601, "message": "method not found"},
            })
        );
    }

    #[test]
    fn id_echoed_as_string_when_client_sends_string_id() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "abc-123",
            "method": "ping",
        })
        .to_string();
        let resp = handle_message(&raw, &[], &mut FakeHandler).unwrap();
        assert_eq!(resp["id"], json!("abc-123"));
    }
}
