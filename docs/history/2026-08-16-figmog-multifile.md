# figmog multi-file serve Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Spec §14 — `figmog serve` becomes a multi-file server: every local tool takes an optional `file` (URL/key), files auto-mirror on first reference, `figmog_open`/`figmog_files` tools, round-robin watch.

**Architecture:** Spec §14 of `docs/superpowers/specs/2026-08-15-figmog-build-design.md` is the binding authority — read it in full. Core move: extract each mirrored file into a `FileSession` whose unnameable store is captured in boxed closures (the crate's established pattern); serve routes by resolving the `file` arg to a session.

**Tech Stack:** No new deps.

**Spec:** docs/superpowers/specs/2026-08-15-figmog-build-design.md §14

## Global Constraints
- Zero new crates. All 145 existing tests green unchanged EXCEPT: the serve e2e's tools/list count assertions move 17→19 and any initialize-instructions assertion must keep passing (the sentence is appended, existing substring stays) — those specific edits are authorized; list every test edit in the report.
- Single-file behavior preserved: `figmog serve <one-file>` behaves exactly as today (default file = that file; no `file` arg needed on any tool).
- Stdout purity, determinism (session listing sorted by open order; `figmog_files` output deterministic), clean EOF exit unchanged.
- Gates: `cargo test -p figmog`, `cargo clippy -p figmog --no-deps -- -D warnings`, `cargo fmt -p figmog --check`.
- Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: FileSession manager + routing + new tools + multiplexed watch

**Files:**
- Create: `examples/figmog/src/sessions.rs`; Modify: `src/serve.rs`, `src/cli.rs` (Serve takes `files: Vec<String>` positional + hidden `--figmog-root <dir>` flag defaulting ".figmog" used for session store paths — testability per spec §14), `src/mcp.rs` (only the INSTRUCTIONS const gains the spec's exact appended sentence + its pinning test updated), `src/dispatch.rs` if needed.
- Test: unit tests in sessions.rs; serve.rs registry test updated 17→19.

**Interfaces:**
- `sessions::FileSession { key: String, name: String, dispatch: Box<dyn FnMut(&str, &Value) -> Result<mcp::ToolOutput, String>>, pull: Box<dyn FnMut() -> Result<PullOutcome, String>>, watermark: Box<dyn FnMut() -> Option<String>>, watcher: Watcher, backoff: Duration }` — built by `sessions::open_session(root: &Path, key: &str, api_token: Option<&str>, pull_now: bool) -> Result<FileSession, String>`; the closure factory owns the `open_store!` concrete site (move the existing per-tool rtx dispatch + inline pull + eviction blocks INTO the factory, generalized from serve.rs's current single-store code — this is a refactor-move, behavior preserved).
- `sessions::SessionManager { sessions: Vec<FileSession>, root, token }` with `resolve(&mut self, file_arg: Option<&str>) -> Result<&mut FileSession, String>` implementing spec §14's resolution rules (explicit → find-or-auto-open; omitted → default rules incl. the isError text naming figmog_open/figmog_files); `open(&mut self, file_ref) -> Result<&mut FileSession, String>` (parse_file_ref, dedupe by key); `list(&self) -> Value` for figmog_files.
- serve.rs: registry gains `figmog_open {file required}` + `figmog_files {}` (19 local tools; input schemas per pattern); every existing local ToolDef's schema gains optional `file` string property (description: "Figma file URL or key; omit for the default mirrored file"); the FnHandler closure extracts `file` from args (removing it before tool-specific arg parsing) and routes through SessionManager::resolve. figmog_sync syncs the RESOLVED session. Watch tick: round-robin — keep one `next_session_idx`; per tick poll ONE session's watcher (deadline = max(interval / session_count, 2s) per spec); Changed → that session's pull(); backoff discipline per session. Zero sessions + watch → just idle ticks (no polling).
- CLI Serve: `files: Vec<String>` positional (zero or more); startup opens each (pull if store empty); first = default. `--figmog-root` threaded to SessionManager (and to resolve_db? NO — CLI single-file semantics unchanged; the flag only affects serve's sessions).

- [ ] **Step 1 (TDD):** sessions unit tests with a scripted closure factory (no real stores): resolve explicit/default/none-mirrored error text; dedupe; auto-open counts. RED → implement sessions.rs → GREEN.
- [ ] **Step 2:** refactor serve.rs onto SessionManager (single-file path first — all existing tests must pass unchanged here, except none should need edits at this step), then add file-arg routing + the two new tools + schema additions + instructions sentence (now the 17→19 and instructions test edits land, listed in the report).
- [ ] **Step 3:** multiplexed watch tick + gates + commit `feat(figmog): multi-file serve — URL-addressed tools, figmog_open/figmog_files`.

---

### Task 2: multi-file e2e + docs

**Files:**
- Modify: `examples/figmog/tests/serve.rs` (new e2e), `examples/figmog/tests/common/mod.rs` (a second small fixture `fixture_other()` — distinct name "OtherFixture", a few nodes, distinct text), `examples/figmog/README.md`, workspace README bullet.

- [ ] **Step 1 (TDD):** e2e per spec §14 testing: pre-build two stores under a temp `--figmog-root` (via `pull --from-file --db <root>/<key>/db` with two different fixtures — note the CLI's --db is explicit so root layout is constructed by the test), start `serve --no-upstream --no-watch --figmog-root <root>` with BOTH keys as positional args; assert: tools/list has 19 tools and every local tool schema contains the optional `file` property; `figmog_files` lists both (first = default); `figmog_search {query:<word-only-in-other>, file:<other-key>}` hits; same query without `file` misses (proves default routing); `figmog_status {file:<other-key>}` returns the other file's name; `figmog_open {file:"garbagekey1234567890"}` → isError (no token); omitted-file error case NOT triggerable here (a default exists) — cover via a second serve spawn with NO positional files: a tool without `file` → isError naming figmog_open.
- [ ] **Step 2:** README: "Multiple files" section (URL-per-call usage, figmog_open/figmog_files, zero-file startup for `claude mcp add`, proxied-tools caveat verbatim from spec §14); workspace bullet unchanged or +"multi-file". Full gates; commit `feat(figmog): multi-file serve e2e and docs`.

## Self-review checklist
- Spec §14 coverage: surface (file arg/open/files/zero-file startup) → T1+T2 e2e; mechanics (sessions/watch/cache) → T1; caveat + docs → T2; non-goals respected (no cross-file queries, no idle eviction, CLI unchanged).
- Single-file regression safety: T1 Step 2's mid-step gate (existing tests green before feature lands).
