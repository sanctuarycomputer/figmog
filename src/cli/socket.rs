//! Unix-socket control-plane client (spec §1): every read command, `tools`,
//! and `call` become clients of a running `figmog serve` process reachable
//! at `<figmog-root>/serve.sock`, instead of opening the store directly —
//! always-fresh answers, no single-writer lock contention, and (for `call
//! figmog_sync`) a pull that reaches the *owning* process rather than
//! failing against a store `serve` already holds locked.
//!
//! Socket routing only applies when the root is knowable at all: `--db`
//! bypasses the whole multi-file root concept (an arbitrary store path has
//! no `<root>/serve.sock` to speak of), so callers in `super::dispatch`
//! only reach this module when `cli.db.is_none()`. [`DEFAULT_ROOT`] is the
//! CLI's own root — it has no `--figmog-root` override (that's `serve`'s
//! hidden testability knob only), so this constant must match
//! [`crate::serve`]'s own default figmog-root exactly for socket routing to
//! ever find the right process.
//!
//! Every function here returns `None` (not `Some(Err(..))`) for "no serve
//! reachable at this root" — a plain connection failure — so the caller
//! falls back to a direct store open exactly as if the socket didn't
//! exist. Once a connection succeeds, `Some(Ok(_))`/`Some(Err(_))` is
//! authoritative: the call reached serve and the caller must not also
//! attempt a direct open (spec §1: "Socket reachable ⇒ the command becomes
//! a client of serve").

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// The CLI's multi-file root when `--db` was not passed — always `.figmog`
/// relative to the current directory, matching `crate::serve::run_serve`'s
/// own `--figmog-root` default (see this module's doc comment).
pub(super) const DEFAULT_ROOT: &str = ".figmog";

/// `<root>/serve.sock` — must match [`crate::serve::socket_path`] exactly.
fn socket_path(root: &Path) -> PathBuf {
    root.join("serve.sock")
}

/// A running serve answers a `tools/call` against its own already-open
/// store near-instantly; anything slower means something's wrong (a
/// wedged process, a huge query) and the CLI should time out rather than
/// hang indefinitely.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(30);

/// Send one `tools/call` for `tool`/`args` over `<root>/serve.sock` and
/// translate the response into the same `Result<Value, String>` shape
/// every direct-mode command already returns (spec §1: "same JSON on
/// stdout, same error shapes"). See this module's doc comment for the
/// `None` vs `Some` contract.
pub(super) fn try_call(root: &Path, tool: &str, args: Value) -> Option<Result<Value, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    Some(call_over(stream, tool, args))
}

/// Like [`try_call`], but for `tools/list` — used by `figmog tools`'s own
/// socket routing to report the exact merged registry the running serve
/// would expose (its own upstream connection, not this invocation's
/// `--upstream`/`--no-upstream` flags — see this module's doc comment:
/// once the socket is reachable, the command is answered by serve's own
/// state).
pub(super) fn try_tools_list(root: &Path) -> Option<Result<Vec<Value>, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    Some(tools_list_over(stream))
}

/// Build the `figmog tools` row shape (`{name, source, cacheable}`) from a
/// raw `tools/list` result — shared by the socket path above and, in
/// principle, any future caller that already has a tool-def array in hand.
pub(super) fn tools_rows(tools: &[Value]) -> Value {
    let rows: Vec<Value> = tools
        .iter()
        .map(|t| {
            let name = t["name"].as_str().unwrap_or_default();
            json!({
                "name": name,
                "source": if crate::proxy::is_local_tool(name) { "local" } else { "upstream" },
                "cacheable": crate::proxy::tool_name_cache_capable(name),
            })
        })
        .collect();
    Value::Array(rows)
}

/// Like [`try_call`], but for `figmog_images` specifically (v0.0.2 spec
/// §5): returns the raw MCP `result` object (`{"content": [...],
/// "isError": ...}`) rather than [`interpret_call_response`]'s unwrapped
/// domain `Value`. `figmog_images`'s content mixes a text manifest block
/// with `image` (base64) blocks — `interpret_call_response`'s "parse
/// `content[0]`'s text as figmog's own JSON" contract only fits the
/// single-text-block shape every other local tool returns, so
/// `cli::images` (the only caller) needs the whole array to decode the
/// image blocks and write files itself.
pub(super) fn try_images_call(root: &Path, args: Value) -> Option<Result<Value, String>> {
    let stream = UnixStream::connect(socket_path(root)).ok()?;
    let resp = send_and_recv(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "figmog_images", "arguments": args},
        }),
    );
    Some(resp.and_then(|resp| {
        if let Some(err) = resp.get("error") {
            return Err(protocol_error_message(err));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }))
}

fn call_over(stream: UnixStream, tool: &str, args: Value) -> Result<Value, String> {
    let resp = send_and_recv(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        }),
    )?;
    interpret_call_response(resp)
}

fn tools_list_over(stream: UnixStream) -> Result<Vec<Value>, String> {
    let resp = send_and_recv(
        stream,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )?;
    if let Some(err) = resp.get("error") {
        return Err(protocol_error_message(err));
    }
    Ok(resp["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// One newline-delimited JSON-RPC frame out, one line back — the wire
/// mechanics every call in this module shares. `initialize` is
/// deliberately skipped: spec §1 makes it optional for socket clients, and
/// `mcp::handle_message` treats every frame independently regardless of
/// handshake state (verified by `serve.rs`'s socket-loop tests), so a bare
/// `tools/call`/`tools/list` is answered exactly as if a handshake had
/// happened first.
fn send_and_recv(stream: UnixStream, frame: &Value) -> Result<Value, String> {
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writeln!(writer, "{frame}").map_err(|e| format!("writing to serve socket: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("writing to serve socket: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|e| {
        // Review m2: a timed-out read (the timeout set above) reads as a
        // generic OS error otherwise (`"Resource temporarily unavailable"`
        // or similar, depending on platform) — spell out what actually
        // happened and the escape hatch, rather than leaving the operator
        // to guess.
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            format!(
                "figmog serve did not respond within {}s (is it wedged? try --no-socket)",
                SOCKET_TIMEOUT.as_secs()
            )
        } else {
            format!("reading from serve socket: {e}")
        }
    })?;
    if n == 0 {
        return Err("serve socket closed before responding".to_string());
    }
    serde_json::from_str(&line).map_err(|e| format!("serve socket sent invalid JSON: {e}"))
}

fn protocol_error_message(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_string()
}

/// Pure translation of one `tools/call` JSON-RPC response into the plain
/// `Result<Value, String>` shape every CLI command already produces —
/// split from [`call_over`]'s I/O so it's unit-testable directly against
/// hand-built response `Value`s.
fn interpret_call_response(resp: Value) -> Result<Value, String> {
    if let Some(err) = resp.get("error") {
        return Err(protocol_error_message(err));
    }
    let result = resp.get("result").cloned().unwrap_or(Value::Null);
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result["content"][0]["text"].as_str().map(str::to_string);

    if is_error {
        return Err(text.unwrap_or_else(|| "unknown error from serve".to_string()));
    }

    match text {
        // Every local `figmog_*` tool — the only kind this CLI ever calls
        // over the socket — answers as `ToolOutput::Json`, i.e. one text
        // content block holding figmog's own JSON (see `mcp::ToolOutput`'s
        // doc comment); parse it back out rather than handing the caller a
        // JSON *string* nested inside the MCP envelope.
        Some(t) => serde_json::from_str(&t)
            .map_err(|e| format!("serve returned non-JSON tool content: {e}")),
        // No text content at all: a `Raw` (proxied, non-`figmog_*`) result.
        // `figmog call` can legitimately name a proxied tool, so this is a
        // real path, not defensive dead code — pass the whole result
        // through unchanged, same as direct mode's own proxy_call return.
        None => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_call_response_parses_json_tool_content() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "{\"ok\":true}"}], "isError": false},
        });
        assert_eq!(interpret_call_response(resp).unwrap(), json!({"ok": true}));
    }

    #[test]
    fn interpret_call_response_is_error_becomes_err_with_the_message_text() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"content": [{"type": "text", "text": "no node 99:99 in the mirror"}], "isError": true},
        });
        assert_eq!(
            interpret_call_response(resp).unwrap_err(),
            "no node 99:99 in the mirror"
        );
    }

    #[test]
    fn interpret_call_response_protocol_error_becomes_err() {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found"},
        });
        assert_eq!(
            interpret_call_response(resp).unwrap_err(),
            "method not found"
        );
    }

    #[test]
    fn interpret_call_response_raw_result_with_no_text_content_passes_through() {
        // A proxied (non-figmog_*) tool's result has no single text block
        // (e.g. an image content type) — passed through as-is rather than
        // failed as "non-JSON tool content".
        let raw_result = json!({
            "content": [{"type": "image", "data": "YQ==", "mimeType": "image/png"}],
            "isError": false,
        });
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": raw_result});
        assert_eq!(interpret_call_response(resp).unwrap(), raw_result);
    }

    #[test]
    fn tools_rows_classifies_local_vs_upstream_and_cacheable() {
        let tools = vec![
            json!({"name": "figmog_status"}),
            json!({"name": "get_code"}),
            json!({"name": "set_selection"}),
        ];
        let rows = tools_rows(&tools);
        let rows = rows.as_array().unwrap();
        assert_eq!(
            rows[0],
            json!({"name": "figmog_status", "source": "local", "cacheable": false})
        );
        assert_eq!(
            rows[1],
            json!({"name": "get_code", "source": "upstream", "cacheable": true})
        );
        assert_eq!(
            rows[2],
            json!({"name": "set_selection", "source": "upstream", "cacheable": false})
        );
    }
}
