# figmog bench --interactive (REPL) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** The interactive REPL mode of spec §13 "Interactive mode" — requests visible as they fire, live API side-by-side, session percentiles.

**Architecture:** Spec §13 "Interactive mode (`--interactive`)" is the binding authority — read it in full. One new module `src/repl.rs` driven from `bench::run`'s setup (reuse the corpus/real-file setup, serve child spawn, frame pump, and derived query pool exactly as built — refactor shared pieces out of `bench.rs` rather than duplicating; the one-shot path must keep behaving identically).

**Tech Stack:** No new deps. `std::io::IsTerminal` for TTY detection; raw ANSI escapes; plain `stdin().lines()` (no readline).

**Spec:** docs/superpowers/specs/2026-08-15-figmog-build-design.md §13

## Global Constraints
- Zero new crates. One-shot bench behavior and ALL existing tests unchanged. `--interactive` + `--json` = usage error (exit 1, message on stderr).
- Non-TTY stdout → no ANSI codes (the e2e depends on this). EOF or `quit` → child reaped, clean exit 0.
- Gates: `cargo test -p figmog`, `cargo clippy -p figmog --no-deps -- -D warnings`, `cargo fmt -p figmog --check`.
- Commit trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

---

### Task 1: the REPL (module + wiring + tests + README)

**Files:**
- Create: `examples/figmog/src/repl.rs`; Modify: `src/bench.rs` (extract/reuse setup + frame-pump + query-pool helpers; make the pieces `pub(crate)`), `src/lib.rs`, `src/cli.rs` (`--interactive` flag on Bench + the json-conflict check), `examples/figmog/README.md` (Demo section gains the REPL walkthrough), workspace README bullet unchanged.
- Test: unit tests for the command parser (line → parsed command, incl. bad input errors) and the latency-line formatter (plain mode); one scripted e2e in `tests/cli.rs` piping `help\nstats\nsearch garden\nrun 20\nreport\nquit\n` into `figmog bench --nodes 300 --interactive` (non-TTY → plain output): assert exit 0, output contains the per-request lines (e.g. `figmog_search`), a `run` burst of 20 numbered lines, a report table, no ANSI escape bytes (`\x1b` absent), and the serve child exits (no zombie — process table not asserted, rely on guard + clean exit).

**Interfaces:**
- `repl::run(session: &mut BenchSession, real_file: Option<RealFileCtx>) -> Result<(), String>` where `BenchSession` is the extracted struct owning the serve child + pump + derived query pool + cumulative `Vec<(tool, Duration)>` stats; `RealFileCtx { key, api: UreqApi }` enables the `api …` commands.
- Command enum + `parse_line(&str) -> Result<Command, String>` (unit-tested): Help, Quit, Run(usize), Report, Api(ApiCmd), Call{tool, args}, Tool{name, args} for the shorthands per spec's list (each shorthand builds the tool's JSON args; `where <pointer> [value]` value parsed as JSON with bare-word→string fallback like the CLI's --equals; `node <id> children` sets children:true).
- Latency line: `#{seq:>4}  {tool:<18} {args_digest:<32} {ms:>8.2}ms  {digest}` — args digest truncated at 32 chars; result digest = hits count for array results, `name` for node results, `isError` text (red) for errors. Color thresholds per spec (10ms green / 100ms yellow / else red) via a `fn paint(s, color, tty)` helper.
- `run N`: reuse the one-shot mixed-workload rotation, print each line as fired, then a burst percentile table (reuse the existing ToolStats/percentile code).
- `report`: same table over the session's cumulative stats.
- `api node <id>` / `api meta`: call the existing UreqApi helpers, print with an `API` tag + "spent 1 Tier-1/Tier-3 call" note; 429 prints Retry-After in red and continues.

- [ ] **Step 1 (TDD):** parser + formatter unit tests RED → implement → GREEN.
- [ ] **Step 2:** extract `BenchSession` from bench.rs (one-shot path re-verified: full `cargo test -p figmog` green before proceeding).
- [ ] **Step 3:** repl loop + CLI wiring + json-conflict error; manual smoke (pipe commands, eyeball output); scripted e2e added and green.
- [ ] **Step 4:** README walkthrough (commands table + a short sample transcript from a real run, incl. the `api node` side-by-side framing for real-file mode).
- [ ] **Step 5:** full gates; commit `feat(figmog): interactive bench REPL — watch requests fire live`.

## Self-review checklist
- Spec §13 interactive coverage: every listed shorthand parses; run/report/api/call/help/quit; color TTY-gating; json-conflict; EOF clean exit. Non-goals respected (no readline). One-shot path byte-identical behavior (existing tests prove).
