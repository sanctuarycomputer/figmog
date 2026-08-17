# figmog serve (MCP server) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `figmog serve` — an MCP stdio server over the figmog store with integrated background sync, so agents query the mirror through MCP tools with always-fresh data.

**Architecture:** Spec §11 of `docs/superpowers/specs/2026-08-15-figmog-build-design.md` (read it first — it is the binding authority). Three moves: extract the CLI's read logic into a shared `query` module returning JSON; implement a dependency-free JSON-RPC/MCP protocol core; run one main loop that owns the store, answering requests between poll ticks.

**Tech Stack:** No new dependencies. JSON-RPC hand-rolled over `serde_json`; threads + `std::sync::mpsc` for the stdin reader; `std::process` in tests.

**Spec:** docs/superpowers/specs/2026-08-15-figmog-build-design.md (§11)

## Global Constraints

- Zero new crates in Cargo.toml. Stdout in serve mode carries ONLY newline-delimited JSON-RPC frames; all logging to stderr.
- All v1 behavior unchanged: every existing test keeps passing without modification (except moves of test-internal imports if a type relocates). The CLI's human/JSON output is byte-identical.
- Determinism rules hold (sorted outputs, no HashMap iteration at boundaries).
- Gates per task: `cargo test -p figmog`, `cargo clippy -p figmog --no-deps -- -D warnings`, `cargo fmt -p figmog --check` all clean. Zero diff to fold/ese/anny.
- If `cargo` is not on PATH, prefix commands with `export PATH="$HOME/.cargo/bin:$PATH" && `.
- Commits end with the `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer.

---

### Task 1: `query.rs` — extract read logic from the CLI

**Files:**
- Create: `examples/figmog/src/query.rs`
- Modify: `examples/figmog/src/cli.rs`, `examples/figmog/src/lib.rs` (add `pub mod query;`)
- Test: existing `tests/cli.rs` must pass unchanged (that IS the test of this refactor)

**Interfaces:**
- Produces `pub fn`s in `figmog::query`, each taking the concrete reader types it needs (same signatures style as today's `cmd_*` — generic over `R: Readable`) and returning `Result<serde_json::Value, String>`:
  - `status(nodes, meta) -> Result<Value, String>` — the object today's `cmd_status` builds
  - `pages(nodes, by_type) -> Result<Value, String>` — JSON array
  - `tree(nodes, children, by_type, id: Option<String>, depth: Option<usize>) -> Result<Value, String>` — move `TreeNode`, `build_tree`, `tree_to_json` here; `TreeNode` and `build_tree` stay `pub` so the CLI's human printer can render from the same structure: also expose `pub fn tree_nodes(...) -> Result<TreeNode, String>` and have `tree()` wrap it
  - `node(nodes, children, id: String, with_children: bool) -> Result<Value, String>`
  - `find(nodes, by_type, node_type: String, page: Option<String>) -> Result<Value, String>`
  - `search(text, nodes, query: &str, limit: usize) -> Result<Value, String>`
  - `instances(nodes, components, component_sets, instances_of, target: &str) -> Result<Value, String>` (move `resolve_component_ids` here as a private fn)
  - `components(nodes, components, component_sets) -> Result<Value, String>`
  - `styles(nodes, styles, styled_by, style_type: Option<String>, values: bool) -> Result<Value, String>`
  - `uses(nodes, styled_by, bound_to, id: &str) -> Result<Value, String>`
  - `vars(nodes, variables, variable_collections, id_filter: Option<String>) -> Result<Value, String>`
- Consumed by: cli.rs `cmd_*` (become printers: call `query::*`, then either `println!("{}", serde_json::to_string(...))` in json mode or render human lines from the returned Value/TreeNode exactly as today), and by Task 3's tool handler.

- [ ] **Step 1:** Create `query.rs` by MOVING the body logic of each `cmd_*` read function (everything between reader access and printing) plus `TreeNode`/`build_tree`/`tree_to_json`/`resolve_component_ids` out of `cli.rs`. The JSON each function returns is exactly the Value the old code printed in `--json` mode. Human rendering stays in `cli.rs`, rebuilt from the returned Value (or `TreeNode` for tree). Node-id normalization (`normalize_node_id`) stays at the CLI/tool boundary — `query::*` receives already-normalized ids EXCEPT where today's code normalizes internally; preserve today's exact behavior.
- [ ] **Step 2:** Rewrite each `cmd_*` in `cli.rs` as a thin printer over `query::*`. Doc-comment `query.rs` (module: "One source of truth for every read answer — shared by the CLI printers and the MCP tools.").
- [ ] **Step 3:** Run the full suite: `cargo test -p figmog`. Every existing test must pass WITHOUT edits — if a cli test fails, the refactor changed behavior; fix the refactor, not the test. Then clippy + fmt gates.
- [ ] **Step 4:** Commit: `refactor(figmog): extract query layer shared by CLI and MCP`

---

### Task 2: `mcp.rs` — protocol core

**Files:**
- Create: `examples/figmog/src/mcp.rs`
- Modify: `examples/figmog/src/lib.rs` (add `pub mod mcp;`)
- Test: unit tests in `mcp.rs`

**Interfaces:**
- Produces:
  ```rust
  /// One registered tool: metadata for tools/list.
  pub struct ToolDef {
      pub name: &'static str,
      pub description: &'static str,
      /// JSON Schema for the tool's arguments.
      pub input_schema: serde_json::Value,
  }
  /// Executes a tools/call. Ok(v) => success content; Err(msg) => isError content.
  pub trait ToolHandler {
      fn call(&mut self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String>;
  }
  /// Handle one incoming JSON-RPC message. Returns the response frame to
  /// write, or None for notifications (and for malformed input handled
  /// via the returned parse-error frame — see below).
  pub fn handle_message(
      raw: &str,
      tools: &[ToolDef],
      handler: &mut dyn ToolHandler,
  ) -> Option<serde_json::Value>;
  pub const SERVER_NAME: &str = "figmog";
  ```
- Behavior contract (unit-test each):
  - Parse failure → `Some({jsonrpc:"2.0", id: null, error:{code:-32700, message:"parse error"}})`.
  - `initialize` → result `{protocolVersion: <echo the client's, or "2025-06-18" if absent>, capabilities: {tools: {}}, serverInfo: {name: "figmog", version: env!("CARGO_PKG_VERSION")}, instructions: <the exact steering text from spec §11 "Relationship to Figma's official MCP server" point 2>}`.
  - `notifications/initialized` (and any method starting `notifications/`) → `None`.
  - `ping` → result `{}`.
  - `tools/list` → `{tools: [{name, description, inputSchema}...]}` from the `ToolDef` slice, in slice order. (The protocol core is registry-agnostic; the real 17-tool registry arrives in Task 4.)
  - `tools/call` with `{name, arguments}` → invoke handler; Ok(v) → result `{content: [{type:"text", text: serde_json::to_string(&v)}], isError: false}`; Err(msg) → result `{content:[{type:"text", text: msg}], isError: true}`. Unknown tool name → handler returns Err (Task 3 handler) — but `mcp.rs` itself must also map a `name` missing from `tools` to the same isError shape without calling the handler.
  - Any other method with an `id` → error `-32601` "method not found". Requests without `id` (notifications) → `None`.
- [ ] **Step 1:** Write the failing unit tests for every bullet above (scripted `&str` → expected `Value` assertions; a `NullHandler` test double returning `Ok(json!({"ok":true}))` / `Err("boom")` by tool name).
- [ ] **Step 2:** Verify compile failure, implement, iterate to green. Gates.
- [ ] **Step 3:** Commit: `feat(figmog): MCP protocol core (JSON-RPC over stdio frames)`

---

### Task 3: structural query pack (query fns + CLI subcommands)

**Files:**
- Modify: `examples/figmog/src/query.rs`, `examples/figmog/src/cli.rs`
- Test: extend `examples/figmog/tests/cli.rs`

**Interfaces:**
- Consumes: Task 1's `query.rs` layout and reader-type conventions.
- Produces five new `query::*` functions (same `Result<Value, String>` convention) and five CLI subcommands, per spec §11's "whole-file structural queries" table (the spec table is the authority for inputs/outputs):
  - `query::stats(nodes, components, component_sets, styles, variables, by_type)` — counts by node type (from iterating nodes, sorted by type name), counts per page (page_id → n, sorted), table totals, text-node count, max depth (walk parent chains or recurse via children — iterate nodes computing depth by following `parent_id` with memoization-free repeated walks; the file is local, O(n·depth) is fine).
  - `query::path(nodes, id)` — follow `parent_id` to the root, then reverse: `[{id, name, type}]` root-first. Unknown id → Err.
  - `query::text(nodes, by_type, page: Option<String>)` — `by_type.search("TEXT")`, look up, optional page filter, sorted by id, `[{id, characters, page_id}]` (characters from `NodeRec.text`).
  - `query::where_(nodes, pointer: &str, equals: Option<Value>, page: Option<String>)` — full scan of `nodes.iter()`; parse each `raw`, `raw.pointer(pointer)`; match = pointer resolves AND (equals absent OR JSON-equal); rows `[{id, name, type, page_id, value}]` sorted by id. Pointer must start with `/` → else Err.
  - `query::at(nodes, x: f64, y: f64)` — scan nodes with `abs_bounds = Some([bx, by, w, h])` where `bx <= x < bx+w && by <= y < by+h`; sort by area (w*h) ascending then id; `[{id, name, type, page_id, area}]`.
- CLI: `figmog stats`, `figmog path <id>`, `figmog text [--page <id>]`, `figmog where --pointer </p> [--equals <json>] [--page <id>]` (`--equals` parsed with `serde_json::from_str`, falling back to treating the bare word as a JSON string so `--equals VERTICAL` works), `figmog at --x N --y N`. All support `--json`; human output follows the existing row-printing conventions; node ids normalized where they're inputs (`path`).

- [ ] **Step 1 (TDD):** extend `tests/cli.rs` with a test asserting against fixture_v1 facts: `stats` — `by_type.TEXT == 1`, `by_page["0:1"] == 4` (1:1, 1:2, 1:3, 1:9), totals `{components: 3, component_sets: 1, styles: 2}`, `max_depth == 3` (document→canvas→frame→text); `path 1-2` → ids `["0:0","0:1","1:1","1:2"]`; `text` → one row, characters "Welcome to the garden"; `where --pointer /layoutMode --equals VERTICAL` → `["1:1"]`; `where --pointer /style/fontSize --equals 32.0` → `["1:2"]`; `at --x 10 --y 10` → includes `1:1` (bounds 0,0,800×400) and excludes nodes without bounds. Run to verify failure.
- [ ] **Step 2:** implement `query::*` + CLI wiring; iterate to green; full gates.
- [ ] **Step 3:** Commit: `feat(figmog): whole-file structural queries (stats/path/text/where/at)`

---

### Task 4: `serve.rs` — the serve loop + CLI wiring

**Files:**
- Create: `examples/figmog/src/serve.rs`
- Modify: `examples/figmog/src/lib.rs` (add `pub mod serve;`), `examples/figmog/src/cli.rs` (add `Serve` variant + dispatch)

**Interfaces:**
- Produces: `pub fn run_serve(db: &crate::cli::Db, file: Option<String>, interval: u64, no_watch: bool) -> Result<(), String>` (make `Db` and the small helpers it needs `pub(crate)`; adjust visibility minimally). CLI: `figmog serve [file] [--interval N (default 10)] [--no-watch]`.
- Loop design (spec §11): spawn a thread reading `stdin` lines into an `mpsc::Sender<String>`; main loop owns the store (opened via `open_store!` at this concrete site) and a `Watcher` seeded from the stored watermark; `recv_timeout(until_next_tick)` — on message: `mcp::handle_message` → write response + `\n` to stdout, flush; on timeout (and `!no_watch`): tick → on `Changed` run the pull sequence inline (fetch via `UreqApi`, `flatten_file`, `collect_sweepable` in `rtx`, `store::sync`), honoring the existing `pull_failure_wait` backoff discipline and watcher-reset-on-failure rule; on `Wait{after}` extend the next deadline. Startup: if the store has no meta row and `no_watch` is false, do an initial pull before serving. eprintln! one startup line (name, file key, watch on/off).
- Tool registry: the **17 `figmog_*` tools** from spec §11's two tables (12 core + 5 structural), descriptions stating "reads the local mirror (no Figma API cost)" vs `figmog_sync`'s "fetches from Figma (spends Tier-1 rate budget)". The `initialize` response carries the spec's steering `instructions` text (Task 2 contract). Handler: match tool name → normalize ids (`normalize_node_id` where the arg is a node id) → `st.rtx(|readers| query::*(…))` → the returned Value. `figmog_sync` → the inline pull sequence → churn JSON. Unknown args types → Err(msg).
- [ ] **Step 1:** Implement `serve.rs` + wire the CLI variant. Keep every closure at the concrete `open_store!` site (the pipeline type is unnameable — same pattern as `dispatch`).
- [ ] **Step 2:** `cargo test -p figmog` (all green — no new tests yet), clippy, fmt. Manual smoke: `printf '…initialize…\n…tools/list…\n' | cargo run -p figmog -- serve --no-watch --db <fixture db>` shows two frames on stdout.
- [ ] **Step 3:** Commit: `feat(figmog): figmog serve — MCP stdio server with integrated sync`

---

### Task 5: end-to-end serve test + docs

**Files:**
- Create: `examples/figmog/tests/serve.rs`
- Modify: `examples/figmog/README.md`, workspace `README.md` (figmog bullet mentions MCP)

**Interfaces:** none new.

- [ ] **Step 1:** Write `tests/serve.rs`: build a fixture DB (reuse the `pull --from-file` pattern from `tests/cli.rs` — copy the `fixture_db()` helper or share via `tests/common`), then `std::process::Command` the compiled binary (`assert_cmd::cargo::cargo_bin("figmog")` gives the path) with `serve --no-watch --db <db>`, piped stdio. Write frames, read responses line-by-line with a read timeout guard (wrap reader thread + channel, or set a generous `wait_with_output` after closing stdin — closing stdin must terminate the loop: reader thread sees EOF, sender drops, `recv_timeout` returns Disconnected → clean exit; implement that exit path in Task 3 if missing). Assertions:
  - initialize response echoes id 1, `serverInfo.name == "figmog"`, and a non-empty `instructions` string mentioning "official Figma MCP"
  - `tools/list` returns exactly 17 tools, all named `figmog_*`, incl. `figmog_search`, `figmog_where`, `figmog_sync`
  - `tools/call figmog_search {query:"garden"}` → `isError:false`, text parses to JSON whose first hit id is `1:2`
  - `tools/call figmog_node {id:"1-2"}` → normalized, `name == "Title"`
  - `tools/call figmog_where {pointer:"/layoutMode", equals:"VERTICAL"}` → one row, id `1:1`
  - `tools/call figmog_node {id:"99:99"}` → `isError:true`
  - unknown method → error `-32601`; unknown tool → `isError:true`
- [ ] **Step 2:** README: new "Use from agents (MCP)" section — what `serve` is (server + built-in sync, one process), the `claude mcp add figmog -- <abs path to>/target/debug/figmog serve <file-url>` snippet (note: build first with `cargo build -p figmog`; or `--db`/`--no-watch` for offline), both tool tables from spec §11 (17 tools), the "radically different from Figma's official MCP" positioning paragraph (namespace, steering instructions, zero capability overlap; only `figmog_sync` spends rate budget), and the five new structural CLI commands added to the command reference table. Workspace README bullet gains "and an MCP server (`figmog serve`)".
- [ ] **Step 3:** Full gates: `cargo test -p figmog` (now incl. serve e2e), clippy, fmt, `cargo test -p fold`, `cargo doc -p figmog --no-deps`.
- [ ] **Step 4:** Commit: `feat(figmog): serve e2e tests and MCP docs`

### Task 6: `upstream.rs` — native-server MCP client

**Files:**
- Create: `examples/figmog/src/upstream.rs`; Modify: `src/lib.rs`
- Test: unit tests in-file + one in-process HTTP fake test

**Interfaces:** per spec §12 "Upstream client": `UpstreamError` (thiserror: Unreachable(String), Protocol(String)), `trait UpstreamMcp { initialize, tools, call }`, `HttpUpstream::new(url: String)`, and `pub struct FakeUpstream` (test-support, `#[cfg(test)]`-adjacent: put it behind `pub` in the module so tests/serve.rs can script it — a Vec of (name, schema) + a closure or queued results for `call`).

- [ ] **Step 1 (TDD):** unit tests: handshake sequence frames (initialize → notifications/initialized → tools/list) built correctly; `call` builds a valid tools/call frame; `application/json` and single-event `text/event-stream` (`data: {...}\n\n`) bodies both parse; `Mcp-Session-Id` response header echoed on subsequent requests; error mapping. In-process HTTP fake: bind `TcpListener` on port 0, spawn a thread answering scripted HTTP responses, point `HttpUpstream` at it, drive initialize+tools/list+call.
- [ ] **Step 2:** implement; gates; commit `feat(figmog): upstream MCP client for the Figma desktop server`.

---

### Task 7: proxy cache records + eviction

**Files:**
- Modify: `examples/figmog/src/model.rs` (Id::ProxyCache, Rec::ProxyCache per spec §12), `src/store.rs` (proxy_cache Table branch in `figmog_pipeline!`; `cache_branch` fn; extend `sync` signature with `evict_cache_before_version: Option<&str>` OR a separate small `evict_stale_cache` helper — pick the design that keeps `sync`'s churn accounting untouched and document it), `src/query.rs` or new `src/cache.rs` (key hashing = stable hash of tool+canonical args — use a simple FNV/deterministic hex of bytes, no new deps; lookup/store helpers taking readers/tx)
- Test: extend `tests/sync.rs` (cache rows survive no-change pulls, evicted on version change; existing churn numbers UNAFFECTED — cache rows must not perturb the probe counts of existing tests, which don't create cache rows)

- [ ] **Step 1 (TDD):** tests: store a cache row via a wtx helper; identical re-pull keeps it (version unchanged); v1→v2 pull evicts it; imported variables still survive both.
- [ ] **Step 2:** implement (model variants, pipeline sink `proxy_cache`, helpers); ALL existing tests must stay green with unchanged assertions; gates; commit `feat(figmog): version-keyed proxy response cache`.

---

### Task 8: serve proxy integration + CLI `tools`/`call` + steering v3

**Files:**
- Modify: `src/serve.rs` (startup probe via `HttpUpstream` unless `--no-upstream`; registry merge per spec §12; route tools/call local-first-else-upstream; cacheable rule (get_*/list_* + explicit node id in args — detect via any arg key in {"nodeId","node_id","id"} with a string value); cache lookup before forward, store after; non-get/list success → immediate meta poll; `figmog_status` gains `upstream` field), `src/mcp.rs` (ONLY the steering-text constant → spec §11 point 3's new verbatim text; update its unit test accordingly — this supersedes the v2 text Task 2 shipped), `src/cli.rs` (`figmog tools`, `figmog call <tool> [--args json]`, `--upstream <url>`, `--no-upstream` on serve)
- Test: unit tests for merge/routing/cacheable-rule with `FakeUpstream`; extend `tests/serve.rs`: one e2e with the in-process HTTP fake upstream (proxied tool listed + round-trips + second identical call served from cache without hitting the fake — assert via the fake's call counter exposed through a side-channel file or header count endpoint)

- [ ] **Step 1 (TDD):** write the failing tests; **Step 2:** implement; gates; commit `feat(figmog): cached proxy — figmog as the single Figma MCP`.

---

### Task 9: Enterprise variables in pull (opportunistic)

**Files:**
- Modify: `src/api.rs` (`fn variables_local(&self, key) -> Result<Option<Value>, ApiError>` on the trait — `Ok(None)` on 403/404), `src/cli.rs` + `src/serve.rs` pull paths (on Some: `parse_variables_export` → extend `flattened.recs` AND the sweepable set with variable/collection ids for THIS pull, per spec §12), README (variables section: Enterprise auto-sync first, plugin export as fallback)
- Test: sync-level test with a fake api returning the fixture export: pull twice → zero churn on variables; remove one variable from the fake's second response → it is swept; 403 fake → import-variables records still survive pulls (v1 behavior intact)

- [ ] **Step 1 (TDD):** failing tests; **Step 2:** implement; gates; commit `feat(figmog): opportunistic Enterprise variables sync in pull`.

---

## Self-review checklist

- Spec §11 coverage: architecture → T4; query refactor → T1; protocol behaviors incl. `instructions` steering → T2 (unit-tested); structural query pack → T3; 17-tool registry → T4 + T5 README; distinct-namespace/steering rule → T2 (initialize) + T4 (names) + T5 (README positioning); testing section → T2 unit / T1 equivalence / T3 cli / T5 e2e. Non-goals respected (no resources/prompts, stdio only; cached-proxy documented as v3, not built).
- The T1 refactor is the risk center: its acceptance gate ("existing tests pass unmodified") is what keeps v1 behavior frozen.
- T4's EOF-exit contract is stated in T5 Step 1 because the test depends on it; implementer of T4 must read T5's step (noted in dispatch).
- Execution order: 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 (T3 before T4 so the registry binds all 17 local tools; T6+T7 before T8; T5's e2e baseline exists before T8 extends it).
- v3 (spec §12) coverage: upstream client → T6; cache records/eviction → T7; registry merge, routing, cacheable rule, steering v3 text (supersedes the v2 text T2 shipped — T8 updates the constant and its unit test), CLI tools/call → T8; Enterprise variables → T9. v3 non-goals respected (no remote-server OAuth proxying, no mid-session re-attach, no selection-call caching).
- T2 shipped the v2 steering text against the then-current spec; the v3 text lands in T8 by design — reviewers should not flag the interim drift (ledgered).
