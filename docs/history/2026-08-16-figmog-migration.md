# figmog standalone migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Execute docs/superpowers/specs/2026-08-16-figmog-standalone-repo.md — figmog at the root of the (already-created, empty) `sanctuarycomputer/figmog` repo, fold via pinned git dep, cuts + shakeout done, CI + release workflow in place, tagged v0.0.1 published pre-release.

**Precondition:** the multi-file serve milestone is complete and pushed on `worktree-figmog` (its final state is what migrates). Do not start while any implementer is active on this worktree.

**Mechanics note (sandbox):** this session's git operations are confined to this worktree. The migration therefore happens on an **orphan branch** here (`figmog-standalone`), whose tree IS the new repo's root layout. **PR flow (user-directed):** first seed the empty repo's `main` with a minimal init commit (LICENSE + one-line README), then push the orphan branch as `first-pass` and open a PR against `main`. All five tasks commit onto that branch (pushed after each task); per-pillar adversarial reviews run against the PR; when clean, the PR merges into `main` and the release tag is cut from `main`. After the final task, switch this worktree back to `worktree-figmog`.

**Version + release (user-directed):** crate version **0.0.1**, tag **v0.0.1**, release **published as a pre-release** (not draft) so the user can download and test the binary. The fold-license caveat remains a README sentence about broader distribution; the user has accepted publishing on their own repo for testing.

**Spec:** docs/superpowers/specs/2026-08-16-figmog-standalone-repo.md (binding; read fully before any task)

## Global Constraints
- The spec's §6 standing rules apply to all new-repo content (determinism, frozen sinks, append-only enums, no fold patches, gates, no new deps).
- Every task ends green IN THE NEW LAYOUT: `cargo test`, `cargo clippy --no-deps -- -D warnings`, `cargo fmt --check` run at the orphan branch root.
- Commit trailer everywhere: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- The clog-fork branch `worktree-figmog` is never modified by this plan.

---

### Task 1: import + git-dep switch (the repo exists after this)

- [ ] **Step 1 — pin determination: DONE by the controller (2026-08-16).** Upstream fetched (`bogkit-upstream` remote exists in this worktree); `fold` at `bogkit-upstream/main` is source-identical to our tree (only a `readme = "readme.md"` metadata line + readme files differ; `anny` likewise readme-only). **The pin is `rev = "20f2ca50d5d06f51edfe8b8570c0fb48caf9eb81"`.** Task 1 Step 3's full-suite gate remains the executable proof. (Upstream still has no LICENSE anywhere — the §3 release gate stands.)
- [ ] **Step 2 — orphan branch + layout:** `git switch --orphan figmog-standalone`; populate from `worktree-figmog`'s tree (use `git restore --source worktree-figmog -- examples/figmog docs` then move): crate files from `examples/figmog/*` to root (src/, tests/, README.md → kept for now, Cargo.toml rewritten standalone: `[package] name figmog version 0.0.1 edition 2024 license MIT` + the git dep `fold = { git = "https://github.com/flowercomputers/bogkit", rev = "<the pin>" }`, same crates.io deps/dev-deps, NO workspace section); `docs/history/` gets the old spec + all figmog plan docs verbatim; LICENSE = MIT (year 2026, copyright sanctuary computer); minimal `.gitignore` (target/, .figmog/). Nothing else yet (CI, SPEC.md, README rewrite are later tasks).
- [ ] **Step 3 — build against the git dep:** `cargo test` at root (network fetch of bogkit occurs here). All 15x tests must pass unchanged — this proves the pin is faithful. Then clippy/fmt gates.
- [ ] **Step 4 — seed main, push branch, open PR:** first seed the empty repo: create a tiny init commit (LICENSE + one-line README stub) on a temp orphan branch and `git push https://github.com/sanctuarycomputer/figmog.git <seed>:main`. Then commit the import on `figmog-standalone` (`import figmog from sanctuarycomputer/clog (branch worktree-figmog, PR #1) as standalone crate` + trailer), `git push https://github.com/sanctuarycomputer/figmog.git figmog-standalone:first-pass`, and `gh pr create -R sanctuarycomputer/figmog --base main --head first-pass` (title "figmog first pass", body summarizing the import + planned task commits; PR body ends with the standard generated-with footer).

### Task 2: the cuts

- [ ] Remove `src/bench.rs`, `src/repl.rs` (+ their lib.rs mods, Cmd::Bench variant + dispatch, bench/repl tests in tests/cli.rs + tests/serve.rs if any, corpus references). Remove `Cmd::Watch` + `cmd_watch` (keep helpers serve/sessions still use — compiler tells). Remove human-mode output: every read command prints `serde_json::to_string_pretty` only; delete the printer/table helpers; remove the `--json` flag (JSON is the only mode) — errors become JSON on stderr unconditionally; update every test that passed `--json` (mechanical flag removal) and any asserting human output (delete those assertions, keep the JSON ones). Spec §4 lists this as approved — the test edits here are authorized wholesale but must be enumerated in the report.
- [ ] Post-cut dead-code sweep: build with `-D warnings` (dead_code surfaces), remove orphans; manual pass over remaining `pub(crate)` items for zero-caller leftovers.
- [ ] Gates; commit `remove bench/REPL/watch and human output mode (JSON-only CLI)`; push.

### Task 3: debt payment (spec §4's list, verbatim scope)

- [ ] Fix each: variable_edges dedup (BTreeSet or remove + honest comment); merge_registry dedup by actual local names; hoist namespace check above per-proxied-call rtx; cache::store surfaces serialize errors (Result); wrong-type args errors say expected/got; `--interval` overflow clamp; serve stdout writes tolerate closed pipe (write! + map_err → clean exit); visibility mismatches (pub mod serve etc.); obj_map borrow instead of clone; README gains Tier-3 budget line (interim — full README rewrite is T4). Add focused tests where a fix changes observable behavior (wrong-type message, interval clamp).
- [ ] Gates; commit `pay down deferred review debt`; push.

### Task 4: structure + docs

- [ ] Split cli.rs (~1.4k lines): `src/cli/mod.rs` (clap types + dispatch + run), `src/cli/pull.rs` (pull/do_pull/PullError/open_store_checked/current helpers), `src/cli/read.rs` (read command fns), `src/cli/call.rs` (tools/call/import-variables). Pure moves; suite green unchanged.
- [ ] `docs/SPEC.md`: consolidated CURRENT-state spec (architecture, data model, pipeline, sync, serve/MCP tools incl. multi-file, proxy + cache, variables story, bench REMOVED — no version archaeology; ~the §2-§14 content that still exists, rewritten present-tense). Old spec stays in docs/history/ untouched.
- [ ] README.md rewrite per spec §2 (standalone: what it is, install from GitHub Releases incl. macOS quarantine note + the fold-license gate sentence while unresolved, quick start, MCP setup incl. multi-file/URL-addressed usage, CLI reference, tool tables, limitations). New-repo CLAUDE.md per spec §6.
- [ ] Gates + `cargo doc --no-deps` warning-free; commit `standalone docs and module structure`; push.

### Task 5: CI + release + tag

- [ ] `.github/workflows/ci.yml`: on push/PR to main — ubuntu + macos runners: `cargo test`, `cargo clippy --no-deps -- -D warnings`, `cargo fmt --check`.
- [ ] `.github/workflows/release.yml` per spec §5: on tag `v*` — matrix {aarch64-apple-darwin on macos-14, x86_64-apple-darwin on macos-13, x86_64-unknown-linux-gnu on ubuntu-latest}; `cargo build --release`; strip; `tar czf figmog-${TAG}-${TARGET}.tar.gz -C target/<triple>/release figmog`; sha256 into SHA256SUMS; `gh release create "$TAG" --prerelease --title "$TAG"` + upload artifacts (use `softprops/action-gh-release` OR plain `gh` CLI — prefer plain `gh`, zero third-party actions beyond actions/checkout + dtolnay/rust-toolchain or rustup manual; document the choice).
- [ ] Push; verify CI runs green on the actual repo (`gh run watch/list -R sanctuarycomputer/figmog`); fix-forward if runner reality differs (allowed: iterative commits, each pushed, until green — list them).
- [ ] Tag `v0.0.1`, push tag, confirm the release appears with 3 artifacts + checksums (`gh release view v0.0.1 -R sanctuarycomputer/figmog`). Publish as PRE-RELEASE (user-directed for testing); README keeps the fold-license sentence for broader distribution.
- [ ] Final: switch this worktree back to `worktree-figmog`. Commit nothing further there.

## Self-review checklist
- Spec §2 layout → T1/T4; §3 dep+pin+gate → T1 (+README sentence T4); §4 cuts+debt → T2/T3; §5 release → T5 (pre-release publish per user direction supersedes the spec's draft-only rule — ledger this as a ruling); §6 CLAUDE.md → T4; §7 sequencing → precondition + task order; §8 non-goals respected (no tap, no signing, no filter-repo).
- Per-pillar adversarial reviews (user-directed) map onto the task reviews: T1 = engine/import fidelity, T2+T3 = CLI surface & hardening, T4 = structure/docs, T5 = supply chain/CI — plus one whole-PR final review before merge.
- Risk center: T1's pin-parity check (fold drift would silently change engine behavior — the full-suite gate at T1 Step 3 is the proof).
