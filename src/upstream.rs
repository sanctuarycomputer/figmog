//! Client for Figma's native desktop MCP server (streamable HTTP).
//!
//! This is figmog's half of the "cached proxy" design in build design §12:
//! figmog probes the local Figma desktop app's Dev Mode MCP server
//! (`http://127.0.0.1:3845/mcp` by default) at startup, forwards calls it
//! doesn't handle itself, and — per §12's cache rules, implemented
//! elsewhere — caches `get_*`/`list_*` responses keyed by file version.
//! This module owns only the wire protocol: MCP's `initialize` handshake,
//! `tools/list`, and `tools/call`, all as JSON-RPC 2.0 frames POSTed to one
//! URL. Registry merge, routing, and caching are Task 8's job (`serve.rs`).
//!
//! [`UpstreamMcp`] is the seam: [`HttpUpstream`] is the real client, and
//! [`FakeUpstream`] is a scripted double other test suites in this crate
//! (e.g. `tests/serve.rs`) can drive without a live desktop app.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Value, json};

/// Errors surfaced by an [`UpstreamMcp`] implementation.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// Transport-level failure: connection refused, DNS failure, timeout —
    /// the upstream server could not be reached at all.
    #[error("upstream unreachable: {0}")]
    Unreachable(String),
    /// The upstream was reached but the exchange was invalid: a malformed
    /// frame, an unparseable response body, or a JSON-RPC `error` member.
    #[error("upstream protocol error: {0}")]
    Protocol(String),
}

/// The three calls figmog's proxy needs from an upstream MCP server.
pub trait UpstreamMcp {
    /// Perform the MCP handshake (`initialize` + `notifications/initialized`)
    /// and populate the tool list returned by [`tools`](Self::tools).
    fn initialize(&mut self) -> Result<(), UpstreamError>;
    /// The tools discovered by the most recent [`initialize`](Self::initialize)
    /// call, verbatim as returned by the upstream's `tools/list`.
    fn tools(&self) -> &[Value];
    /// Invoke `tools/call` on the upstream and return the JSON-RPC `result`
    /// value (the same shape the MCP spec gives a client: typically
    /// `{"content": [...], "isError": bool}`).
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, UpstreamError>;
}

/// MCP protocol version figmog's `initialize` request declares.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Blocking `ureq`-backed [`UpstreamMcp`] against a streamable-HTTP MCP
/// server (Figma's desktop app, by default at
/// `http://127.0.0.1:3845/mcp`).
pub struct HttpUpstream {
    url: String,
    agent: ureq::Agent,
    session_id: Option<String>,
    /// The `protocolVersion` the upstream's `initialize` response actually
    /// negotiated (which may differ from [`PROTOCOL_VERSION`] if the
    /// upstream negotiates down). `None` until `initialize` succeeds; once
    /// set, sent as `MCP-Protocol-Version` on every later request per the
    /// 2025-06-18 streamable-HTTP transport spec.
    protocol_version: Option<String>,
    next_id: u64,
    tools: Vec<Value>,
}

impl HttpUpstream {
    /// A client posting JSON-RPC frames to `url`. Does not connect until
    /// [`initialize`](UpstreamMcp::initialize) or
    /// [`call`](UpstreamMcp::call) is called.
    pub fn new(url: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build();
        HttpUpstream {
            url,
            agent,
            session_id: None,
            protocol_version: None,
            next_id: 1,
            tools: Vec::new(),
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// POST one JSON-RPC frame and return the parsed JSON-RPC response
    /// object (still containing its `result`/`error` wrapper — callers use
    /// [`extract_result`] to unwrap it). Captures `Mcp-Session-Id` from the
    /// response header, if present, for subsequent requests.
    fn send_request(&mut self, body: &Value) -> Result<Value, UpstreamError> {
        let (content_type, text) = self.post(body)?;
        parse_streamable_body(&content_type, &text)
    }

    /// POST a notification (no response body expected): fire-and-forget,
    /// still subject to the session header dance and transport error
    /// mapping, but the body (if any) is discarded rather than parsed.
    fn send_notification(&mut self, body: &Value) -> Result<(), UpstreamError> {
        self.post(body)?;
        Ok(())
    }

    /// Shared transport: POST `body`, capture/refresh the session header,
    /// and return `(content_type, body_text)` for the caller to interpret.
    /// Both success and HTTP-error responses are read the same way — an
    /// MCP server can return a JSON-RPC `error` object on a non-2xx status.
    fn post(&mut self, body: &Value) -> Result<(String, String), UpstreamError> {
        let mut req = self
            .agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(session_id) = &self.session_id {
            req = req.set("Mcp-Session-Id", session_id);
        }
        // Per the 2025-06-18 streamable-HTTP transport spec (the version
        // this client declares, `PROTOCOL_VERSION`), every request after a
        // successful `initialize` must carry the negotiated protocol
        // version; servers may reject requests without it (I-2). Absent
        // before `initialize` completes — there's nothing negotiated yet.
        if let Some(protocol_version) = &self.protocol_version {
            req = req.set("MCP-Protocol-Version", protocol_version);
        }
        let resp = match req.send_json(body.clone()) {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(ureq::Error::Transport(e)) => {
                return Err(UpstreamError::Unreachable(e.to_string()));
            }
        };
        if let Some(session_id) = resp.header("Mcp-Session-Id") {
            self.session_id = Some(session_id.to_string());
        }
        let content_type = resp.content_type().to_string();
        let text = resp
            .into_string()
            .map_err(|e| UpstreamError::Protocol(format!("failed to read response body: {e}")))?;
        Ok((content_type, text))
    }
}

impl UpstreamMcp for HttpUpstream {
    fn initialize(&mut self) -> Result<(), UpstreamError> {
        let id = self.next_request_id();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "figmog", "version": env!("CARGO_PKG_VERSION")},
            },
        });
        let resp = self.send_request(&init_req)?;
        let result = extract_result(resp)?;
        // Capture the negotiated version so every request from here on
        // (including the `notifications/initialized` below) carries it.
        self.protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_string);

        self.send_notification(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))?;

        let list_id = self.next_request_id();
        let list_req = json!({
            "jsonrpc": "2.0",
            "id": list_id,
            "method": "tools/list",
        });
        let resp = self.send_request(&list_req)?;
        let result = extract_result(resp)?;
        self.tools = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                UpstreamError::Protocol("tools/list result missing `tools` array".into())
            })?;
        Ok(())
    }

    fn tools(&self) -> &[Value] {
        &self.tools
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<Value, UpstreamError> {
        let id = self.next_request_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        });
        let resp = self.send_request(&req)?;
        extract_result(resp)
    }
}

/// Unwrap a JSON-RPC response object: `{"error": {...}}` becomes
/// [`UpstreamError::Protocol`]; otherwise the `result` member is returned
/// (missing `result` is itself a protocol error — never a panic).
fn extract_result(resp: Value) -> Result<Value, UpstreamError> {
    if let Some(err) = resp.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream returned an error")
            .to_string();
        return Err(UpstreamError::Protocol(msg));
    }
    resp.get("result")
        .cloned()
        .ok_or_else(|| UpstreamError::Protocol("response has neither `result` nor `error`".into()))
}

/// Parse a streamable-HTTP MCP response body given its Content-Type:
/// `application/json` is a single JSON-RPC object; `text/event-stream` is
/// SSE, whose final `data:` event is the JSON-RPC response (consecutive
/// `data:` lines within one event join with `\n`; events are separated by
/// a blank line). Any other content type, or a body that doesn't parse, is
/// a [`UpstreamError::Protocol`] — never a panic.
fn parse_streamable_body(content_type: &str, body: &str) -> Result<Value, UpstreamError> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "application/json" => serde_json::from_str(body)
            .map_err(|e| UpstreamError::Protocol(format!("invalid JSON body: {e}"))),
        "text/event-stream" => parse_sse_last_event(body),
        other => Err(UpstreamError::Protocol(format!(
            "unexpected content-type: {other}"
        ))),
    }
}

/// Parse an SSE stream and return the last event's `data:` payload as
/// JSON. Events are separated by a blank line; within one event,
/// consecutive `data:` lines join with `\n` per the SSE spec.
fn parse_sse_last_event(body: &str) -> Result<Value, UpstreamError> {
    let normalized = body.replace("\r\n", "\n");
    let mut last: Option<Value> = None;
    for event in normalized.split("\n\n") {
        let mut data_lines = Vec::new();
        for line in event.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if data_lines.is_empty() {
            continue;
        }
        let joined = data_lines.join("\n");
        match serde_json::from_str::<Value>(&joined) {
            Ok(v) => last = Some(v),
            Err(_) => continue,
        }
    }
    last.ok_or_else(|| UpstreamError::Protocol("no JSON-RPC event found in SSE stream".into()))
}

/// A scripted [`UpstreamMcp`] double for tests. `tools` is returned
/// verbatim by [`tools()`](UpstreamMcp::tools); `results` is a FIFO queue
/// consumed one entry per [`call()`](UpstreamMcp::call) — push whatever a
/// test needs the next call(s) to return, in order. `call_count` lets a
/// test assert whether the upstream was hit at all (e.g. a cache-hit test
/// asserting the fake was *not* called a second time).
///
/// `pub` (not `#[cfg(test)]`) so other test binaries in this crate, such
/// as `tests/serve.rs`, can construct and script it directly.
pub struct FakeUpstream {
    pub tools: Vec<Value>,
    pub results: VecDeque<Result<Value, UpstreamError>>,
    pub call_count: usize,
    pub initialize_calls: usize,
}

impl FakeUpstream {
    /// A fake exposing `tools` from `tools/list`, with no scripted call
    /// results yet — push some with [`push_result`](Self::push_result)
    /// before driving any [`call`](UpstreamMcp::call).
    pub fn new(tools: Vec<Value>) -> Self {
        FakeUpstream {
            tools,
            results: VecDeque::new(),
            call_count: 0,
            initialize_calls: 0,
        }
    }

    /// Queue the result the next [`call()`](UpstreamMcp::call) returns.
    pub fn push_result(&mut self, result: Result<Value, UpstreamError>) {
        self.results.push_back(result);
    }
}

impl UpstreamMcp for FakeUpstream {
    fn initialize(&mut self) -> Result<(), UpstreamError> {
        self.initialize_calls += 1;
        Ok(())
    }

    fn tools(&self) -> &[Value] {
        &self.tools
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<Value, UpstreamError> {
        let _ = (name, args);
        self.call_count += 1;
        self.results
            .pop_front()
            .unwrap_or_else(|| Err(UpstreamError::Protocol("no scripted result queued".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    // --- pure helper tests -------------------------------------------------

    #[test]
    fn parses_application_json_body() {
        let v = parse_streamable_body(
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        )
        .unwrap();
        assert_eq!(v, json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}));
    }

    #[test]
    fn parses_application_json_body_with_charset_param() {
        let v = parse_streamable_body(
            "application/json; charset=utf-8",
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        )
        .unwrap();
        assert_eq!(v["id"], json!(1));
    }

    #[test]
    fn parses_single_event_sse_body() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = parse_streamable_body("text/event-stream", body).unwrap();
        assert_eq!(v, json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}));
    }

    #[test]
    fn sse_multiline_data_within_one_event_joins_with_newline() {
        // Per SSE rules, consecutive `data:` lines within one event join
        // with `\n`. Split across two `data:` lines, the JSON is only
        // valid once joined — proving the join actually happens.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1,\ndata: \"b\":2}}\n\n";
        let v = parse_streamable_body("text/event-stream", body).unwrap();
        assert_eq!(v["result"], json!({"a": 1, "b": 2}));
    }

    #[test]
    fn sse_takes_last_event_when_multiple_present() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"first\":true}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"second\":true}}\n\n",
        );
        let v = parse_streamable_body("text/event-stream", body).unwrap();
        assert_eq!(v["result"], json!({"second": true}));
    }

    #[test]
    fn unknown_content_type_is_protocol_error() {
        let err = parse_streamable_body("text/plain", "hello").unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(_)));
    }

    #[test]
    fn malformed_json_body_is_protocol_error_not_panic() {
        let err = parse_streamable_body("application/json", "{not json").unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(_)));
    }

    #[test]
    fn sse_body_with_no_data_lines_is_protocol_error() {
        let err = parse_streamable_body("text/event-stream", "event: ping\n\n").unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(_)));
    }

    #[test]
    fn extract_result_unwraps_result_member() {
        let v = extract_result(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})).unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    #[test]
    fn extract_result_maps_error_member_to_protocol_error() {
        let err = extract_result(json!({
            "jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}
        }))
        .unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(msg) if msg == "boom"));
    }

    #[test]
    fn extract_result_missing_both_members_is_protocol_error_not_panic() {
        let err = extract_result(json!({"jsonrpc":"2.0","id":1})).unwrap_err();
        assert!(matches!(err, UpstreamError::Protocol(_)));
    }

    #[test]
    fn frame_construction_initialize_and_call_shapes() {
        // initialize's params shape, independent of any network I/O.
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "figmog", "version": env!("CARGO_PKG_VERSION")},
            },
        });
        assert_eq!(init["params"]["protocolVersion"], json!("2025-06-18"));
        assert_eq!(init["params"]["clientInfo"]["name"], json!("figmog"));

        let call = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "get_code", "arguments": {"nodeId": "1:2"}},
        });
        assert_eq!(call["method"], json!("tools/call"));
        assert_eq!(call["params"]["name"], json!("get_code"));
        assert_eq!(call["params"]["arguments"]["nodeId"], json!("1:2"));
    }

    // --- FakeUpstream --------------------------------------------------

    #[test]
    fn fake_upstream_serves_scripted_results_in_order_and_counts_calls() {
        let mut fake = FakeUpstream::new(vec![json!({"name": "get_code"})]);
        fake.push_result(Ok(json!({"first": true})));
        fake.push_result(Err(UpstreamError::Protocol("second fails".into())));

        fake.initialize().unwrap();
        assert_eq!(fake.tools().len(), 1);

        assert_eq!(
            fake.call("get_code", &json!({})).unwrap(),
            json!({"first": true})
        );
        assert!(fake.call("get_code", &json!({})).is_err());
        assert_eq!(fake.call_count, 2);
        // Queue exhausted: next call reports a clear scripting error, not a panic.
        assert!(fake.call("get_code", &json!({})).is_err());
        assert_eq!(fake.call_count, 3);
    }

    // --- in-process HTTP fake: full handshake + call -----------------------

    fn read_request(stream: &mut TcpStream) -> (String, String) {
        let mut header_bytes = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read request byte");
            header_bytes.push(byte[0]);
            if header_bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let header_text = String::from_utf8_lossy(&header_bytes).to_string();
        let content_length: usize = header_text
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0);
        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 {
            stream
                .read_exact(&mut body_bytes)
                .expect("read request body");
        }
        (
            header_text,
            String::from_utf8_lossy(&body_bytes).to_string(),
        )
    }

    fn write_response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
        let mut resp = format!("HTTP/1.1 {status}\r\n");
        resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
        // Force a fresh TCP connection per request so this hand-rolled
        // single-shot server never has to multiplex keep-alive requests.
        resp.push_str("Connection: close\r\n");
        for (k, v) in headers {
            resp.push_str(&format!("{k}: {v}\r\n"));
        }
        resp.push_str("\r\n");
        resp.push_str(body);
        stream.write_all(resp.as_bytes()).expect("write response");
        stream.flush().expect("flush response");
    }

    fn request_id(body: &str) -> Value {
        serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .unwrap_or(Value::Null)
    }

    /// Drives `HttpUpstream` through initialize (which itself is two HTTP
    /// requests: `initialize` then the `notifications/initialized`
    /// notification) → `tools/list` → `tools/call`, against a hand-rolled
    /// HTTP/1.1 server on a thread. Asserts: the session id the fake issues
    /// on the `initialize` response is echoed as `Mcp-Session-Id` on every
    /// later request; an `application/json` response and a
    /// `text/event-stream` response both parse.
    #[test]
    fn http_upstream_handshake_and_call_against_fake_server() {
        const SESSION_ID: &str = "sess-abc123";

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let seen_requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_requests_srv = Arc::clone(&seen_requests);

        let server = thread::spawn(move || {
            for i in 0..4u32 {
                let (mut stream, _) = listener.accept().expect("accept");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set_read_timeout");
                let (headers, body) = read_request(&mut stream);
                seen_requests_srv.lock().unwrap().push(headers);

                match i {
                    0 => {
                        // initialize
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": request_id(&body),
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": {"name": "fake-figma-desktop", "version": "1.0"},
                            },
                        })
                        .to_string();
                        write_response(
                            &mut stream,
                            "200 OK",
                            &[
                                ("Content-Type", "application/json"),
                                ("Mcp-Session-Id", SESSION_ID),
                            ],
                            &resp,
                        );
                    }
                    1 => {
                        // notifications/initialized
                        write_response(&mut stream, "202 Accepted", &[], "");
                    }
                    2 => {
                        // tools/list, delivered as a single-event SSE stream
                        let inner = json!({
                            "jsonrpc": "2.0",
                            "id": request_id(&body),
                            "result": {"tools": [
                                {"name": "get_code", "description": "d", "inputSchema": {"type": "object"}},
                            ]},
                        })
                        .to_string();
                        let sse = format!("data: {inner}\n\n");
                        write_response(
                            &mut stream,
                            "200 OK",
                            &[("Content-Type", "text/event-stream")],
                            &sse,
                        );
                    }
                    3 => {
                        // tools/call
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": request_id(&body),
                            "result": {"content": [{"type": "text", "text": "ok"}], "isError": false},
                        })
                        .to_string();
                        write_response(
                            &mut stream,
                            "200 OK",
                            &[("Content-Type", "application/json")],
                            &resp,
                        );
                    }
                    _ => unreachable!(),
                }
            }
        });

        let url = format!("http://{addr}/mcp");
        let mut upstream = HttpUpstream::new(url);
        upstream.initialize().expect("initialize");
        assert_eq!(upstream.tools().len(), 1);
        assert_eq!(upstream.tools()[0]["name"], json!("get_code"));

        let result = upstream
            .call("get_code", &json!({"nodeId": "1:2"}))
            .expect("call");
        assert_eq!(result["content"][0]["text"], json!("ok"));

        server.join().expect("server thread");

        let reqs = seen_requests.lock().unwrap();
        assert_eq!(reqs.len(), 4);
        // The initialize request predates the session id, so it must not
        // carry one yet.
        assert!(!reqs[0].to_ascii_lowercase().contains("mcp-session-id"));
        // Every request after the initialize response must echo it.
        for req in &reqs[1..] {
            assert!(
                req.to_ascii_lowercase()
                    .contains(&format!("mcp-session-id: {SESSION_ID}")),
                "expected Mcp-Session-Id header, got: {req}"
            );
        }

        // I-2: the initialize request predates any negotiated protocol
        // version, so it must not carry the header yet; every request from
        // request 2 onward (notifications/initialized, tools/list,
        // tools/call) must carry the version the fake server's initialize
        // response negotiated ("2025-06-18").
        assert!(
            !reqs[0]
                .to_ascii_lowercase()
                .contains("mcp-protocol-version")
        );
        for req in &reqs[1..] {
            assert!(
                req.to_ascii_lowercase()
                    .contains("mcp-protocol-version: 2025-06-18"),
                "expected MCP-Protocol-Version header, got: {req}"
            );
        }
    }

    #[test]
    fn http_upstream_unreachable_url_is_unreachable_error() {
        // Port 0 as a *target* (not bind) is refused immediately by the OS
        // on every platform we run on, so this never actually blocks.
        let mut upstream = HttpUpstream::new("http://127.0.0.1:0/mcp".to_string());
        let err = upstream.initialize().unwrap_err();
        assert!(matches!(err, UpstreamError::Unreachable(_)));
    }
}
