#![recursion_limit = "256"]

//! End-to-end test of `figmog serve`: spawns the real compiled binary as a
//! child process, drives it over stdin/stdout exactly as an MCP client
//! would, and asserts on the JSON-RPC frames it writes back. Everything
//! else in this crate tests the pieces (`mcp::handle_message` unit tests,
//! CLI smoke tests over `query::*`); this is the one test proving the
//! pieces are wired together correctly in the real process, including the
//! stdin-EOF exit contract `--no-watch` mode relies on.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Generous but bounded: every wait in this test — for a response line or
/// for the child to exit — is capped at this, so a regression that makes
/// the server hang fails the test instead of the test run.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Kills the child on drop so a failed assertion (which unwinds past the
/// rest of the test body, skipping the normal stdin-close/wait sequence)
/// never leaves an orphaned `figmog serve` process behind.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `figmog serve --no-watch --db <db>` with piped stdio. Returns the
/// kill-on-drop guard, a writer for stdin, and a channel of stdout lines
/// fed by a reader thread — driving the child through a channel (rather
/// than reading its stdout inline) means a hung child blocks only the
/// bounded `recv_timeout` in [`recv`], never the test thread itself.
fn spawn_serve(db: &std::path::Path) -> (ChildGuard, ChildStdin, Receiver<String>) {
    spawn_serve_with_args(db, &["--no-upstream"])
}

/// Like [`spawn_serve`], but with extra CLI args after `--db <db>` (e.g.
/// `--upstream <url>` for the proxy e2e test).
///
/// `current_dir` is pinned to `db`'s own tempdir (every caller's `db` is
/// `<tempdir>/db` — see `common::fixture_db`): `figmog serve` binds its
/// unix-socket control plane (spec §1) at `<figmog-root>/serve.sock`, and
/// `--figmog-root` defaults to the cwd-relative `.figmog` regardless of
/// `--db` — without this, every one of this file's `--db`-mode e2e tests
/// (which never sets its own `current_dir`) would default to the *shared*
/// `cargo test` process cwd, racing every other concurrently-running test
/// in this binary to bind the exact same socket path.
fn spawn_serve_with_args(
    db: &std::path::Path,
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--db"])
        .arg(db)
        .args(extra_args)
        .current_dir(db.parent().expect("db path has a parent tempdir"));
    spawn_child(cmd)
}

/// Spawn `figmog serve --no-watch --no-upstream --figmog-root <root>
/// <files...>` (spec §14's multi-file surface, with the hidden
/// `--figmog-root` testability flag pointed at a pre-built fixture root),
/// with `FIGMA_TOKEN` scrubbed from the child's environment — every
/// multi-file e2e that touches `figmog_open` needs the missing-token
/// isError, not whatever real token the test runner's own shell happens to
/// export.
fn spawn_serve_multifile(
    root: &std::path::Path,
    files: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream", "--figmog-root"])
        .arg(root)
        .args(files)
        .env_remove("FIGMA_TOKEN");
    spawn_child(cmd)
}

/// Shared plumbing behind every `spawn_serve*` helper: pipe stdio, spawn,
/// drain stderr for debugging visibility (never asserted on), and feed
/// stdout lines into a channel — driving the child through a channel
/// (rather than reading its stdout inline) means a hung child blocks only
/// the bounded `recv_timeout` in [`recv`], never the test thread itself.
fn spawn_child(mut cmd: Command) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn figmog serve");

    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let stderr = child.stderr.take().expect("child stderr");

    // Drain stderr on its own thread purely for debugging visibility
    // (`serve` logs there, e.g. "figmog serving ..."); never asserted on.
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("[figmog serve stderr] {line}");
        }
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    (ChildGuard(child), stdin, rx)
}

/// Pre-build one fixture store per `(key, fixture)` pair under a fresh temp
/// `--figmog-root` layout — `<root>/<key>/db`, exactly the path
/// [`sessions::open_session`](../src/sessions.rs) derives (spec §14) — each
/// via `figmog pull --from-file`, so the multi-file `serve` e2e can start
/// against pre-populated mirrors without ever touching the network. Returns
/// the tempdir; the caller must keep it alive for the duration of the test.
fn build_fixture_root(entries: &[(&str, Value)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (key, fixture) in entries {
        let resp = dir.path().join(format!("{key}.json"));
        std::fs::write(&resp, serde_json::to_string(fixture).unwrap()).unwrap();
        let db = dir.path().join(key).join("db");
        assert_cmd::Command::cargo_bin("figmog")
            .unwrap()
            .args(["pull", "--from-file"])
            .arg(&resp)
            .arg("--db")
            .arg(&db)
            .assert()
            .success();
    }
    dir
}

/// Bare file keys (spec §14: 10+ alphanumeric chars — see `ident::parse_file_ref`),
/// deliberately readable rather than realistic, for the multi-file e2e
/// tests below. `KEY_A` mirrors [`common::fixture_v1`] and is always the
/// first startup file (so the default-routing rule picks it); `KEY_B`
/// mirrors [`common::fixture_other`].
const KEY_A: &str = "figmogkeyoneaaaa1111";
const KEY_B: &str = "figmogkeytwobbbb2222";

/// Write one JSON-RPC frame, newline-delimited (the protocol this crate's
/// `mcp`/`serve` modules speak).
fn send(stdin: &mut ChildStdin, msg: &Value) {
    writeln!(stdin, "{msg}").expect("write to child stdin");
    stdin.flush().expect("flush child stdin");
}

/// Read and parse the next response line, bounded by [`TIMEOUT`] so a
/// stuck server fails this assertion instead of hanging the test binary.
fn recv(rx: &Receiver<String>) -> Value {
    let line = rx
        .recv_timeout(TIMEOUT)
        .expect("figmog serve did not respond within the timeout");
    serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("response line was not valid JSON: {e}\nline: {line}"))
}

/// Poll `try_wait` instead of a single blocking `wait()`, so a child that
/// never exits fails with a clear panic at `timeout` rather than hanging
/// the test run forever.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("figmog serve did not exit within {timeout:?} of stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn call(stdin: &mut ChildStdin, rx: &Receiver<String>, id: i64, name: &str, args: Value) -> Value {
    send(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": args},
        }),
    );
    recv(rx)
}

/// The tool result's text content, parsed as JSON (every `figmog_*` tool
/// returns `query::*` JSON serialized as the single text content block).
fn result_json(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in: {resp}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("content text not JSON: {e}\ntext: {text}"))
}

#[test]
fn serve_e2e_initialize_tools_list_and_tool_calls() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    // -- initialize --
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    assert_eq!(resp["result"]["serverInfo"]["name"], json!("figmog"));
    let instructions = resp["result"]["instructions"]
        .as_str()
        .expect("instructions is a string");
    assert!(!instructions.is_empty());
    // v3 steering text (build design §12/§11 point 3): figmog is now the
    // only Figma MCP an agent connects to — a cached proxy in front of
    // Figma's native capabilities — which supersedes the v2 "second,
    // separate server" text this assertion used to pin.
    assert!(
        instructions.contains("cached proxy"),
        "instructions should mention the cached proxy: {instructions}"
    );

    // notifications/initialized: no `id`, so no response frame is expected
    // (mirrors a real MCP client's handshake; the server ignores it).
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: exactly 20 figmog_* tools (spec §14: the 17 v3 tools
    // plus figmog_open/figmog_files, plus v0.0.2 §2's figmog_subtree) --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 20, "tools: {tools:#?}");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for name in &names {
        assert!(
            name.starts_with("figmog_"),
            "tool outside the figmog_ namespace: {name}"
        );
    }
    for expected in ["figmog_search", "figmog_where", "figmog_sync"] {
        assert!(names.contains(&expected), "missing tool: {expected}");
    }

    // -- figmog_search: first hit is 1:2 ("Title", text "...garden") --
    let resp = call(
        &mut stdin,
        &rx,
        3,
        "figmog_search",
        json!({"query": "garden"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    assert_eq!(hits[0]["id"], json!("1:2"));

    // -- figmog_node: id normalization (12-34 form) + raw JSON name --
    let resp = call(&mut stdin, &rx, 4, "figmog_node", json!({"id": "1-2"}));
    assert_eq!(resp["result"]["isError"], json!(false));
    let node = result_json(&resp);
    assert_eq!(node["name"], json!("Title"));

    // -- figmog_where: exactly one row, id 1:1 --
    let resp = call(
        &mut stdin,
        &rx,
        5,
        "figmog_where",
        json!({"pointer": "/layoutMode", "equals": "VERTICAL"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!("1:1"));

    // -- figmog_node on an unknown id: isError --
    let resp = call(&mut stdin, &rx, 6, "figmog_node", json!({"id": "99:99"}));
    assert_eq!(resp["result"]["isError"], json!(true));

    // -- unknown JSON-RPC method: -32601 --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 7, "method": "totally/bogus"}),
    );
    let resp = recv(&rx);
    assert_eq!(resp["error"]["code"], json!(-32601));

    // -- unknown tool name: isError, not a protocol-level error --
    let resp = call(&mut stdin, &rx, 8, "figmog_nonexistent", json!({}));
    assert_eq!(resp["result"]["isError"], json!(true));

    // Closing stdin is what makes the (`--no-watch`) serve loop exit: its
    // reader thread sees EOF and drops the sender, so the main loop's
    // blocking `rx.recv()` returns `Disconnected` and the process exits 0.
    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// I-1: while `figmog serve` holds the store's single-writer lock, a CLI
/// command opened against the same `--db` must still exit 1 with figmog's
/// friendly locked-store message on stderr — never fold's raw `unwrap()`
/// panic *exit code* (101). Reproduces the review's live repro (serve
/// holding a fixture store, `figmog status --db <same>` in a second
/// process) as an automated test.
///
/// `open_store_checked` (see its doc comment in `cli/mod.rs`) suppresses the
/// default panic hook for the call, so fold's raw `thread 'main'
/// panicked… ` banner must never reach stderr at all — this test proves
/// that directly by parsing the *entire* stderr buffer as one JSON object
/// (`serde_json::from_slice`, not a substring `contains` check): any
/// leaked banner text before or after the JSON line would make the whole
/// buffer fail to parse.
#[test]
fn cli_read_against_a_store_serve_holds_fails_clean_not_with_a_panic() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    // Complete the handshake before touching the store from a second
    // process: `run_serve` opens the store synchronously, before it can
    // ever respond to `initialize` (see serve.rs), so a response here
    // proves the lock is already held.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["result"]["serverInfo"]["name"], json!("figmog"));

    let out = assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["status", "--db"])
        .arg(&db)
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a clean exit-1, not fold's raw panic exit (101); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr must be exactly one JSON object with no leaked panic banner \
             (parse error: {e}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let error = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an \"error\" string field, got: {parsed}"));
    assert!(
        error.contains("store is locked"),
        "expected the locked-store message, got: {error}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

// ---- cached-proxy e2e: figmog serve against an in-process HTTP fake ----
//
// Minimal hand-rolled HTTP/1.1 server (std `TcpListener`, no new deps) that
// answers exactly the handshake + one `tools/call` `HttpUpstream::initialize`
// and a proxied call make: `initialize`, `notifications/initialized`,
// `tools/list`, then one `tools/call`. Mirrors `upstream.rs`'s own
// in-process fake (same wire mechanics), recreated here because that one
// lives in a `#[cfg(test)]` module private to the lib crate and isn't
// reachable from this integration-test binary.

fn read_request(stream: &mut TcpStream) -> String {
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
    String::from_utf8_lossy(&body_bytes).to_string()
}

fn write_response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &str) {
    let mut resp = format!("HTTP/1.1 {status}\r\n");
    resp.push_str(&format!("Content-Length: {}\r\n", body.len()));
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

/// Spawn a fake upstream MCP server answering exactly 4 requests: the
/// `HttpUpstream::initialize` handshake (`initialize`,
/// `notifications/initialized`, `tools/list` — advertising one tool,
/// `get_code`), then one `tools/call` returning canned content. Returns
/// its address and a join handle; the test drives exactly one real
/// `tools/call` through the child, so a second, cache-served call never
/// reaches this server — proven by the fake never accepting a 5th
/// connection (the accept loop simply ends).
fn spawn_fake_upstream() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = std::thread::spawn(move || {
        for i in 0..4u32 {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set_read_timeout");
            let body = read_request(&mut stream);
            match i {
                0 => {
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
                        &[("Content-Type", "application/json")],
                        &resp,
                    );
                }
                1 => {
                    write_response(&mut stream, "202 Accepted", &[], "");
                }
                2 => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": request_id(&body),
                        "result": {"tools": [
                            {
                                "name": "get_code",
                                "description": "Returns code for a node",
                                "inputSchema": {"type": "object", "properties": {"nodeId": {"type": "string"}}},
                            },
                        ]},
                    })
                    .to_string();
                    write_response(
                        &mut stream,
                        "200 OK",
                        &[("Content-Type", "application/json")],
                        &resp,
                    );
                }
                3 => {
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": request_id(&body),
                        "result": {"content": [{"type": "text", "text": "CODE_HERE"}], "isError": false},
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
    (format!("http://{addr}/mcp"), handle)
}

#[test]
fn serve_e2e_proxied_tool_lists_round_trips_and_second_call_is_cache_served() {
    let (_dir, db) = common::fixture_db();
    let (fake_addr, fake_handle) = spawn_fake_upstream();
    let (mut guard, mut stdin, rx) = spawn_serve_with_args(&db, &["--upstream", &fake_addr]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: 20 local + 1 proxied, prefixed description --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 21, "tools: {tools:#?}");
    let proxied = tools
        .iter()
        .find(|t| t["name"] == json!("get_code"))
        .expect("get_code should be in the merged registry");
    assert_eq!(
        proxied["description"],
        json!("[via Figma desktop] Returns code for a node")
    );

    // -- figmog_status: upstream connected --
    let resp = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp)["upstream"], json!("connected"));

    // -- first get_code call with an explicit nodeId: round-trips the
    // fake's canned content verbatim (the proxied result is a complete MCP
    // `CallToolResult` already — figmog emits it as-is, NOT re-wrapped in
    // another text content block, so native output formats like an image
    // block survive unmangled) and is cacheable (get_* + string nodeId). --
    let resp = call(&mut stdin, &rx, 4, "get_code", json!({"nodeId": "1:2"}));
    assert_eq!(
        resp["result"],
        json!({"content": [{"type": "text", "text": "CODE_HERE"}], "isError": false})
    );

    // -- second identical call: served from the version-keyed cache — the
    // fake upstream server only ever accepts 4 connections total (the
    // handshake's 3 plus this test's one real `tools/call`), so if this
    // call reached the network the fake's accept loop would still be
    // blocked waiting for a 5th connection and `fake_handle.join()` below
    // would hang past the test harness's own timeout.
    let resp2 = call(&mut stdin, &rx, 5, "get_code", json!({"nodeId": "1:2"}));
    assert_eq!(
        resp2["result"], resp["result"],
        "second identical call should be served byte-identically from cache"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    fake_handle
        .join()
        .expect("fake upstream server thread should finish after exactly 4 requests");
}

// ---- multi-file serve e2e (spec §14) ----
//
// Two pre-built stores under a temp `--figmog-root` (built via `pull
// --from-file --db <root>/<key>/db`, never touching the network), started
// with BOTH keys as positional args, proves the whole v4 surface: 20 tools,
// every local tool's optional `file` schema property, `figmog_files`,
// `file`-argument routing to a *specific* mirror, default-file routing on
// an omitted `file`, and `figmog_open`'s isError on a missing token. A
// second spawn with zero startup files covers the omitted-`file`-with-no-
// default error text a single default file can never trigger.

#[test]
fn serve_e2e_multi_file_routes_by_file_arg_and_first_startup_key_is_default() {
    let root = build_fixture_root(&[
        (KEY_A, common::fixture_v1()),
        (KEY_B, common::fixture_other()),
    ]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[KEY_A, KEY_B]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // -- tools/list: 20 tools; every tool but figmog_open/figmog_files
    // carries an *optional* `file` property, figmog_open's `file` is
    // required, figmog_files takes none (spec §14). --
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let resp = recv(&rx);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 20, "tools: {tools:#?}");
    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        let schema = &tool["inputSchema"];
        let required: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
        match name {
            "figmog_open" => {
                assert!(
                    schema["properties"]["file"].is_object(),
                    "figmog_open should take a file property"
                );
                assert!(
                    required.contains(&json!("file")),
                    "figmog_open's file should be required"
                );
            }
            "figmog_files" => {
                assert!(
                    schema["properties"].get("file").is_none(),
                    "figmog_files should not take a file argument"
                );
            }
            _ => {
                assert!(
                    schema["properties"]["file"].is_object(),
                    "{name} is missing the optional file routing property"
                );
                assert!(
                    !required.contains(&json!("file")),
                    "{name}'s file property must be optional"
                );
            }
        }
    }

    // -- figmog_files: both mirrors, in open order, KEY_A (first startup
    // FILE) is the default. --
    let resp = call(&mut stdin, &rx, 3, "figmog_files", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("files array");
    assert_eq!(rows.len(), 2, "files: {rows:#?}");
    assert_eq!(rows[0]["key"], json!(KEY_A));
    assert_eq!(rows[0]["name"], json!("Fixture"));
    assert_eq!(rows[0]["default"], json!(true));
    assert_eq!(rows[1]["key"], json!(KEY_B));
    assert_eq!(rows[1]["name"], json!("OtherFixture"));
    assert_eq!(rows[1]["default"], json!(false));

    // -- figmog_search {query: "zephyr", file: KEY_B} hits: "zephyr" only
    // appears in fixture_other's one TEXT node, so a hit here proves the
    // `file` argument actually reached the *other* mirror. --
    let resp = call(
        &mut stdin,
        &rx,
        4,
        "figmog_search",
        json!({"query": "zephyr", "file": KEY_B}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    let hits = hits.as_array().expect("hits array");
    assert!(!hits.is_empty(), "expected a 'zephyr' hit in {KEY_B}");
    assert_eq!(hits[0]["id"], json!("1:1"));

    // -- same query with `file` omitted routes to the default (KEY_A /
    // fixture_v1, which never mentions "zephyr" anywhere) and misses —
    // proving omission really does route to the default session rather
    // than reusing whichever mirror answered the previous call. --
    let resp = call(
        &mut stdin,
        &rx,
        5,
        "figmog_search",
        json!({"query": "zephyr"}),
    );
    assert_eq!(resp["result"]["isError"], json!(false));
    let hits = result_json(&resp);
    assert!(
        hits.as_array().expect("hits array").is_empty(),
        "default file should have no 'zephyr' hits: {hits:?}"
    );

    // -- figmog_status {file: KEY_B}: the *other* file's own name. --
    let resp = call(&mut stdin, &rx, 6, "figmog_status", json!({"file": KEY_B}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp)["name"], json!("OtherFixture"));

    // -- figmog_open {file: "garbagekey1234567890"}: a brand-new key
    // auto-opens (no pull yet — see sessions::SessionManager::open), then
    // figmog_open's own pull fails cleanly because FIGMA_TOKEN is scrubbed
    // from this child's environment (spawn_serve_multifile). --
    let resp = call(
        &mut stdin,
        &rx,
        7,
        "figmog_open",
        json!({"file": "garbagekey1234567890"}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Zero startup files (spec §14: valid, token-free, idle startup) — the
/// only shape in which the omitted-`file`-with-no-default error is
/// triggerable at all (a single startup file, or an established default,
/// always resolves the omitted case; two auto-opened mirrors are covered
/// by `sessions.rs`'s own unit tests). Proves the error names both new
/// tools, per spec §14's resolution rule.
#[test]
fn serve_e2e_multi_file_zero_startup_omitted_file_errors_naming_figmog_open() {
    let root = tempfile::tempdir().unwrap();
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // figmog_files: nothing mirrored yet.
    let resp = call(&mut stdin, &rx, 2, "figmog_files", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false));
    assert_eq!(result_json(&resp), json!([]));

    // A tool call with `file` omitted and no default mirrored file:
    // isError naming figmog_open/figmog_files, not a silent empty answer.
    let resp = call(&mut stdin, &rx, 3, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("figmog_open"),
        "error should name figmog_open: {text}"
    );
    assert!(
        text.contains("figmog_files"),
        "error should name figmog_files: {text}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// spec §2b: a full Figma frame URL works directly as `id` on
/// `figmog_node`/`figmog_subtree`, and — with no explicit `file` argument
/// and no startup default (zero startup files here) — the URL's own file
/// key auto-opens the right mirror (spec §14's existing auto-open
/// semantics), exactly as if `file: KEY_A` had been passed explicitly.
/// The mirror's store already exists on disk (pre-built by
/// `build_fixture_root`), so this never touches the network.
#[test]
fn serve_e2e_zero_startup_url_id_infers_the_file_and_resolves_the_node() {
    let root = build_fixture_root(&[(KEY_A, common::fixture_v1())]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // Nothing mirrored yet — the store on disk hasn't been opened as a
    // session until a tool call names it.
    let resp = call(&mut stdin, &rx, 2, "figmog_files", json!({}));
    assert_eq!(result_json(&resp), json!([]));

    let url = format!("https://www.figma.com/design/{KEY_A}/Fixture?node-id=1-1&t=abc-1");

    // figmog_node: `id` alone (no `file`) resolves the Hero frame from
    // KEY_A's mirror.
    let resp = call(&mut stdin, &rx, 3, "figmog_node", json!({"id": url}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");
    let node = result_json(&resp);
    assert_eq!(node["id"], json!("1:1"));
    assert_eq!(node["name"], json!("Hero"));

    // figmog_subtree: same URL, still no `file` — proves the auto-open
    // isn't a one-shot fluke of the first call above.
    let resp = call(
        &mut stdin,
        &rx,
        4,
        "figmog_subtree",
        json!({"id": url, "depth": 0}),
    );
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");
    let subtree = result_json(&resp);
    assert_eq!(subtree["id"], json!("1:1"));
    assert_eq!(subtree["children"], json!([]));

    // The URL's file key really did auto-open a session, not answer from
    // thin air.
    let resp = call(&mut stdin, &rx, 5, "figmog_files", json!({}));
    let rows = result_json(&resp);
    let rows = rows.as_array().expect("files array");
    assert_eq!(rows.len(), 1, "files: {rows:#?}");
    assert_eq!(rows[0]["key"], json!(KEY_A));

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Review fix I3: the explicit-`file`/URL-file mismatch note (spec §2b)
/// must compare *normalized* file keys, not raw argument text — an
/// explicit `file` that's itself a full Figma URL naming the very same
/// file the `id` URL names is not a disagreement, even though the two
/// strings look nothing alike. A genuinely different explicit `file` still
/// gets the note.
#[test]
fn serve_e2e_mismatch_note_compares_normalized_file_keys() {
    let root = build_fixture_root(&[
        (KEY_A, common::fixture_v1()),
        (KEY_B, common::fixture_other()),
    ]);
    let (mut guard, mut stdin, rx) = spawn_serve_multifile(root.path(), &[]);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    // Same file, different spelling: explicit `file` is a full URL naming
    // KEY_A, `id` is a *different* full URL also naming KEY_A (with an
    // unknown node id, so the call still fails) — no mismatch, no note.
    let same_file_url = format!("https://www.figma.com/design/{KEY_A}/Alt-Name");
    let id_url_a = format!("https://www.figma.com/file/{KEY_A}/Fixture?node-id=99-99");
    let resp = call(
        &mut stdin,
        &rx,
        2,
        "figmog_node",
        json!({"id": id_url_a, "file": same_file_url}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("99:99"), "text: {text}");
    assert!(
        !text.contains("note:"),
        "same file, different spelling: no mismatch note expected — text: {text}"
    );

    // Genuinely different files: explicit `file` names KEY_A, `id` is a
    // URL naming KEY_B — the note must appear and name both keys.
    let id_url_b = format!("https://www.figma.com/design/{KEY_B}/Other?node-id=99-99");
    let resp = call(
        &mut stdin,
        &rx,
        3,
        "figmog_node",
        json!({"id": id_url_b, "file": KEY_A}),
    );
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("note:"), "text: {text}");
    assert!(text.contains(KEY_A), "text: {text}");
    assert!(text.contains(KEY_B), "text: {text}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// `--db <path>` with no FILE positional predates multi-file serve (spec
/// §14 non-goal: CLI multi-file addressing is out of scope) — the single
/// session it opens has no real Figma key to pull with (see
/// `sessions::open_session_at`'s `network_key: None` case). Any tool that
/// forces a pull must fail with the same clean pre-v4 message, never panic
/// or attempt a network call against a filesystem-path-shaped "key".
#[test]
fn serve_e2e_db_override_with_no_file_figmog_sync_errors_no_file_key() {
    let (_dir, db) = common::fixture_db();
    let (mut guard, mut stdin, rx) = spawn_serve(&db);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    let resp = call(&mut stdin, &rx, 2, "figmog_sync", json!({}));
    assert_eq!(resp["result"]["isError"], json!(true));
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("no file key"),
        "expected the no-file-key message, got: {text}"
    );
    assert!(
        !text.to_lowercase().contains("panic"),
        "must not panic: {text}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// Documents an accepted divergence (see this crate's README, "Multiple
/// files" section, and `serve.rs::build_sessions`'s doc comment):
/// `.figmog/current` is only refreshed by a startup pull that actually
/// *ran* (`!no_watch && !session.mirrored`). `--no-watch` never pulls at
/// startup — not even against a pre-built store under the default
/// `.figmog` root — so `figmog serve <key> --no-watch` (no `--db`) must
/// NOT write `.figmog/current`, even though pre-v4 `figmog serve <key>`
/// (which always watched) did. A real pull that writes it (an initial
/// watch-mode pull against an empty store, or a later watch-tick pull)
/// needs a live network + token and is deliberately not exercised here —
/// this test only pins the `--no-watch` half, which is fully offline.
#[test]
fn serve_e2e_no_watch_default_root_startup_does_not_write_figmog_current() {
    let cwd = tempfile::tempdir().unwrap();
    let response = cwd.path().join("resp.json");
    std::fs::write(
        &response,
        serde_json::to_string(&common::fixture_v1()).unwrap(),
    )
    .unwrap();
    let db = cwd.path().join(".figmog").join(KEY_A).join("db");
    assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();

    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream", KEY_A])
        .current_dir(cwd.path())
        .env_remove("FIGMA_TOKEN");
    let (mut guard, mut stdin, rx) = spawn_child(cmd);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(&rx);
    assert_eq!(resp["id"], json!(1));
    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    assert!(
        !cwd.path().join(".figmog").join("current").exists(),
        "--no-watch startup against a pre-built store must not write .figmog/current"
    );
}

// ---- unix-socket control plane e2e (v0.0.2 spec §1) ----
//
// The CLI's own socket routing (`cli::socket`) only ever tries a root
// derived exactly like `figmog serve`'s default: `.figmog` relative to the
// current directory (it has no `--figmog-root` override — that's serve's
// hidden testability knob). Every test below builds a root-derived fixture
// store at `<cwd>/.figmog/<key>/db` (the same layout a real `figmog pull`
// would leave) plus `<cwd>/.figmog/current`, then runs both `figmog serve`
// and every CLI probe with `current_dir(cwd)` so the two sides agree on
// the root.

/// Build a root-derived fixture store at `<dir>/.figmog/<key>/db` — the
/// exact path `sessions::open_session` derives for a startup FILE with no
/// `--figmog-root` override — and establish `<dir>/.figmog/current`
/// pointing at it (which `pull --from-file --db <path>` itself never
/// writes, since `--db` short-circuits key tracking — see
/// `cli::resolve_db`), matching what a real `figmog pull <url>` run from
/// `dir` would leave behind. Returns the tempdir; the caller must keep it
/// alive for the test's duration.
fn default_root_fixture(key: &str, fixture: Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let response = dir.path().join("resp.json");
    std::fs::write(&response, serde_json::to_string(&fixture).unwrap()).unwrap();
    let figmog_dir = dir.path().join(".figmog");
    let db = figmog_dir.join(key).join("db");
    assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["pull", "--from-file"])
        .arg(&response)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();
    std::fs::write(figmog_dir.join("current"), key).unwrap();
    dir
}

/// Spawn `figmog serve --no-watch --no-upstream <key>` against the default
/// (cwd-relative) `.figmog` root, with `FIGMA_TOKEN` scrubbed — every
/// socket-plane e2e test below runs the CLI half from the same `cwd`, so
/// both sides derive the identical root/db paths a real deployment would.
fn spawn_default_root_serve(
    cwd: &std::path::Path,
    key: &str,
    extra_args: &[&str],
) -> (ChildGuard, ChildStdin, Receiver<String>) {
    let bin = assert_cmd::cargo::cargo_bin("figmog");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--no-watch", "--no-upstream", key])
        .args(extra_args)
        .current_dir(cwd)
        .env_remove("FIGMA_TOKEN");
    spawn_child(cmd)
}

/// The `initialize` / `notifications/initialized` handshake every stdio
/// e2e test in this file performs before its first real request — shared
/// here since the socket-plane tests below still drive `figmog serve` over
/// stdio (to prove it's alive and to close it down cleanly) alongside the
/// socket-routed CLI calls under test.
fn handshake(stdin: &mut ChildStdin, rx: &Receiver<String>) {
    send(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}},
        }),
    );
    let resp = recv(rx);
    assert_eq!(resp["id"], json!(1));
    send(
        stdin,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
}

/// (a) `figmog status` while `figmog serve` owns the store: reachable over
/// the socket ⇒ fresh data, exit 0, no lock error at all (spec §1's
/// headline fix — this is the exact friction the whole feature exists to
/// remove).
#[test]
fn socket_status_returns_fresh_data_with_no_lock_error() {
    const KEY: &str = "figmogsocketkeyaaa111";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .arg("status")
        .current_dir(dir.path())
        .assert()
        .success();
    let stdout = out.get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{}", String::from_utf8_lossy(&stdout)));
    assert!(v.get("error").is_none(), "unexpected error shape: {v}");
    assert_eq!(v["name"], json!("Fixture"));
    assert!(
        v["nodes"].as_u64().unwrap_or(0) > 0,
        "expected a nonzero node count: {v}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (b) `figmog call figmog_sync` reaches the *owning* process over the
/// socket rather than failing against the local lock. `FIGMA_TOKEN` is
/// scrubbed from `serve`'s own environment (`spawn_default_root_serve`),
/// so its pull attempt fails with a specific, recognizable error — proving
/// the error genuinely came from serve's own sync attempt (not a local
/// store-lock panic, which this CLI invocation could never hit anyway,
/// since it never opens the store directly when the socket is reachable)
/// — and serve must stay alive and answering afterward.
#[test]
fn socket_call_figmog_sync_reaches_the_owning_process_not_a_local_lock_error() {
    const KEY: &str = "figmogsocketkeybbb222";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["call", "figmog_sync"])
        .current_dir(dir.path())
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected a clean exit-1 JSON error, not a lock panic; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr must be exactly one JSON object (parse error: {e}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let err = parsed["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an \"error\" string field, got: {parsed}"));
    assert!(
        err.contains("FIGMA_TOKEN not set"),
        "expected serve's own sync error, got: {err}"
    );
    assert!(
        !err.to_lowercase().contains("locked"),
        "must not be a local store-lock error: {err}"
    );

    // serve is still alive and answering after that error.
    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (c) `--no-socket` forces the old direct-open path even with a reachable
/// serve — reproducing the pre-v0.0.2 lock error exactly, on both the
/// `serve` side (never listens) and the CLI side (never probes).
#[test]
fn no_socket_flag_forces_the_old_lock_error() {
    const KEY: &str = "figmogsocketkeyccc333";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["status", "--no-socket"])
        .current_dir(dir.path())
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(output.status.code(), Some(1));
    let parsed: Value = serde_json::from_slice(&output.stderr).unwrap();
    let err = parsed["error"].as_str().unwrap();
    assert!(
        err.contains("store is locked"),
        "expected the direct-mode lock error, got: {err}"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (d) A clean stdin-EOF exit removes `serve.sock` — the socket file must
/// not outlive the process that created it.
#[test]
fn clean_exit_removes_serve_sock() {
    const KEY: &str = "figmogsocketkeyddd444";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let sock_path = dir.path().join(".figmog").join("serve.sock");
    assert!(
        sock_path.exists(),
        "socket file should exist while serve is running"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");

    assert!(
        !sock_path.exists(),
        "socket file should be removed on clean exit"
    );
}

/// (e) A second `figmog serve` against the same root, while the first is
/// still alive, exits with the clean JSON "another figmog serve owns this
/// root" error — never stealing the socket. Started with zero startup
/// files so its own session-store open (a separate concern, spec §1's I-1)
/// can never race the socket check and mask it with a store-lock error
/// instead.
#[test]
fn second_serve_on_same_root_exits_with_clean_error() {
    const KEY: &str = "figmogsocketkeyeee555";
    let dir = default_root_fixture(KEY, common::fixture_v1());
    let (mut guard, mut stdin, rx) = spawn_default_root_serve(dir.path(), KEY, &[]);
    handshake(&mut stdin, &rx);

    let out = assert_cmd::Command::cargo_bin("figmog")
        .unwrap()
        .args(["serve", "--no-watch", "--no-upstream"])
        .current_dir(dir.path())
        .env_remove("FIGMA_TOKEN")
        .assert()
        .failure();
    let output = out.get_output();
    assert_eq!(output.status.code(), Some(1));
    let parsed: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr must be exactly one JSON object (parse error: {e}); stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let err = parsed["error"].as_str().unwrap();
    assert!(
        err.contains("another figmog serve owns this root"),
        "expected the clean multi-owner error, got: {err}"
    );

    // The first serve is untouched by the second's failed startup.
    let resp = call(&mut stdin, &rx, 2, "figmog_status", json!({}));
    assert_eq!(resp["result"]["isError"], json!(false), "resp: {resp:#?}");

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}

/// (f) A stale socket file (nothing listening behind it — the simplest
/// reproduction of a leftover from an unclean exit) is unlinked and
/// rebound at startup, proven by a real client successfully connecting to
/// the *new* socket afterward.
#[test]
fn stale_socket_file_is_unlinked_and_rebound() {
    let dir = tempfile::tempdir().unwrap();
    let figmog_dir = dir.path().join(".figmog");
    std::fs::create_dir_all(&figmog_dir).unwrap();
    let sock_path = figmog_dir.join("serve.sock");
    std::fs::write(&sock_path, b"not a socket, nothing listening here").unwrap();

    let (mut guard, mut stdin, rx) =
        spawn_default_root_serve(dir.path(), "figmogstalekeyfff666", &[]);
    handshake(&mut stdin, &rx);

    assert!(sock_path.exists(), "a real socket should now be bound");
    assert!(
        std::os::unix::net::UnixStream::connect(&sock_path).is_ok(),
        "the rebound socket should accept a real connection"
    );

    drop(stdin);
    let status = wait_with_timeout(&mut guard.0, TIMEOUT);
    assert!(status.success(), "figmog serve exited with {status:?}");
}
