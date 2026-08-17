# figmog bench Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox syntax.

**Goal:** `figmog bench` — the self-contained load-test demo of spec §13: synthetic corpus → timed cold sync → timed zero-churn re-pull → MCP serve load over real stdio → percentile report.

**Architecture:** Spec §13 of `docs/superpowers/specs/2026-08-15-figmog-build-design.md` is the binding authority — read it first. One new module `src/bench.rs` (corpus generator + phase runners + report types), one CLI subcommand wiring, README demo section.

**Tech Stack:** No new dependencies. Seeded LCG for determinism; `std::process` + `current_exe()` for the serve phase; `Instant` timing; sort-based percentiles.

**Spec:** docs/superpowers/specs/2026-08-15-figmog-build-design.md §13

## Global Constraints

- Zero new crates. Determinism: same `--nodes` → byte-identical corpus JSON (unit-tested). No wall-clock/randomness in the generator (LCG seed is a constant).
- Stdout purity: human table OR `--json` object, never both; diagnostics to stderr. Exit nonzero on any phase failure or any `isError` tool result.
- Temp dir cleaned unless `--keep` (which prints the path). Bench never touches the network.
- Existing 112 tests stay green unchanged. Gates: `cargo test -p figmog`, `cargo clippy -p figmog --no-deps -- -D warnings`, `cargo fmt -p figmog --check`.
- Commits end with the `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer.

---

### Task 1: the whole bench (module + CLI + tests + README)

**Files:**
- Create: `examples/figmog/src/bench.rs`; Modify: `src/lib.rs` (`pub mod bench;`), `src/cli.rs` (Bench subcommand + dispatch), `examples/figmog/README.md` (Demo section), workspace `README.md` (bullet mentions the bench)
- Test: unit tests in `bench.rs` + one e2e smoke in `tests/cli.rs`

**Interfaces:**
- `bench::generate_corpus(nodes: usize) -> serde_json::Value` — a full GET-file-shaped response. Structure: ~1 CANVAS page per 250 nodes; per page: auto-layout FRAMEs each containing TEXT children (characters = 4-8 words from a fixed 64-word const list, LCG-picked); one COMPONENT_SET ("Button", 2 variant COMPONENT children with `componentPropertyDefinitions`) on the first page; every ~20th frame child is an INSTANCE with `componentId` pointing at a variant and `componentProperties`; two styles (S:1 FILL, S:2 TEXT) referenced by ~each frame/text via `styles`; every ~10th frame carries a `boundVariables` fill binding to one of 3 variable ids; every node has `absoluteBoundingBox` laid out on a grid. Node ids `"p:i"` scheme, name pool from the word list. Exact node count == `nodes` (count nodes as you emit; stop precisely — unit-tested).
- `bench::run(opts: BenchOpts) -> Result<BenchReport, String>` where `BenchOpts { nodes, calls, keep, exe: PathBuf }` and `BenchReport` (Serialize) carries: corpus {nodes, bytes, gen_ms}, cold {flatten_ms, sync_ms, records, records_per_s}, repull {ms, churn_zero: bool}, load {per_tool: Vec<ToolStats {tool, calls, p50_ms, p95_ms, p99_ms, max_ms}>, total_calls, wall_s, req_per_s}. Phases per spec §13: cold sync + re-pull via the LIBRARY (flatten_file + open_store! + store::sync at a concrete site in bench.rs — same pattern as cli.rs); serve load by spawning `opts.exe` with `["serve","--no-upstream","--no-watch","--db",…]`, doing initialize + notifications/initialized, then M rotating tools/call frames (mix per spec: search w/ rotating words, node w/ LCG-picked real ids, where /layoutMode==VERTICAL, stats, tree depth 2, instances "Button"), timing write→response-line with Instant. Kill child via guard on drop; close stdin at end for clean exit.
- CLI: `Bench { #[arg(long, default_value="10000")] nodes: usize, #[arg(long, default_value="5000")] calls: usize, #[arg(long)] keep: bool }` — note bench does NOT need a resolved db/mirror: handle it BEFORE resolve_db in dispatch (like nothing else needs the Db) — `exe` = `std::env::current_exe()`. `--json` global flag prints `serde_json::to_string_pretty(&report)`; human mode prints the phase lines + per-tool table + headline: `figmog served {total} queries in {wall:.1}s ({req_per_s:.0} req/s). Figma's Tier-1 API budget on a free plan: ~10 file requests per MINUTE.`
- Percentiles: sort the per-tool Vec<Duration>; pN = v[((n-1) * N / 100)] (document the convention).

- [ ] **Step 1 (TDD, generator):** unit tests first: exact node count for 100/1000/10007 (odd number); byte-identical JSON across two calls; contains ≥1 COMPONENT_SET, ≥1 INSTANCE, TEXT nodes with non-empty characters; flatten_file succeeds on it and yields exactly `nodes` node records. Run RED, implement generator, GREEN.
- [ ] **Step 2 (phases):** implement cold-sync/re-pull phases (assert churn zero in-code, else Err) and the serve-load phase + report assembly. Unit-test percentile math on a known vector.
- [ ] **Step 3 (wiring + e2e):** CLI subcommand + dispatch (before resolve_db); e2e smoke in tests/cli.rs: `figmog bench --nodes 300 --calls 60 --json` → exit 0, stdout parses as JSON, `repull.churn_zero == true`, `load.total_calls == 60`, every per-tool p50 ≥ 0. Keep it <30s in CI (300 nodes is plenty).
- [ ] **Step 4 (docs):** README "Demo: load-testing the server" section — the one command (`cargo run --release -p figmog -- bench`), what it does (4 phases), sample output block (from a real run on this machine, dev profile is fine — note profile), the headline framing. Workspace README figmog bullet gains "with a built-in load-test demo (`figmog bench`)".
- [ ] **Step 5:** full gates, commit `feat(figmog): figmog bench — self-contained load-test demo` + trailer.

## Self-review checklist
- Spec §13 coverage: corpus determinism → Step 1; phases/report/headline → Steps 2-4; constraints (no deps, stdout purity, cleanup, nonzero exit) → all steps; non-goals respected (sequential single pipe; no proxy benchmarking).
