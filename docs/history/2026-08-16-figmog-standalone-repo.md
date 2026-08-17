# figmog standalone repo — migration & overhaul design

**Date:** 2026-08-16
**Status:** Approved in discussion; this document is the binding record.
**From:** `sanctuarycomputer/clog` branch `worktree-figmog`, `examples/figmog`
**To:** new repo `sanctuarycomputer/figmog`, crate at root

## 1. Goal

figmog graduates from a hackathon example crate to a standalone,
distributable tool: its own repo, `fold` pulled via git, dead weight
removed, and downloadable binaries on GitHub Releases. Homebrew is
explicitly deferred (no tap, no formula — revisit later).

## 2. New repo shape

```
figmog/
  Cargo.toml            — single crate (bin + lib), version 0.1.0
  src/                  — moved wholesale from examples/figmog/src (minus cuts)
  tests/                — moved wholesale (minus cuts)
  docs/
    SPEC.md             — consolidated current-state spec (no v1..v5 archaeology)
    history/…           — the old build-design spec + plans, verbatim, read-only
  README.md             — rewritten standalone (install via release binary,
                          quick start, MCP setup, tool tables, limitations)
  CLAUDE.md             — figmog's standing rules (see §6)
  LICENSE               — MIT, covering figmog's own code
  .github/workflows/
    ci.yml              — test + clippy(-D warnings, --no-deps) + fmt on push/PR
    release.yml         — see §5
```

History: clean start — one import commit whose message records provenance
(source repo, branch, PR #1 URL). No filter-repo.

## 3. Dependencies

```toml
fold = { git = "https://github.com/flowercomputers/bogkit", rev = "<pinned>" }
```

- `anny` rides along via fold's internal path-dep; everything else stays
  crates.io as today (serde, serde_json, ureq 2, clap 4, thiserror 2;
  dev: tempfile, assert_cmd, postcard).
- The rev is pinned and bumped deliberately. Documented fallback: retarget
  to the `sanctuarycomputer/clog` fork if upstream moves or archives.
- **License gate:** `fold` currently has no license (bogkit repo has no
  LICENSE file, fold's Cargo.toml no license field). figmog's own code is
  MIT, and building locally is fine — but **publishing release binaries
  that embed fold waits until upstream adds a license**. The release
  workflow lands ready; the first public (non-draft) release is a manual
  act taken after that clears. README states this plainly until resolved.

## 4. Cuts and shakeout

**Removed surfaces (approved):**
- `bench.rs`, `repl.rs`, the corpus generator, `--interactive` — the whole
  bench/demo apparatus (~2.5k lines) and its spec §13 (moves to history).
- `figmog watch` CLI command (serve owns sync; `pull` remains for
  one-shots). Its helpers survive only where serve/sessions use them.
- Human-mode CLI output: every read command emits JSON only; the `--json`
  flag disappears (JSON is the only mode); errors are JSON on stderr.
  `serve`'s MCP protocol output is unchanged (it was already pure frames).

**Kept:** pull (`--from-file`, `--fresh`), all read commands incl. the
structural pack, import-variables + inference (variables fallbacks stay),
tools/call, serve (multi-file + desktop proxy + Enterprise variables).

**Debt paid during migration** (the deferred-minors ledger, plus fresh
audit):
- `variable_edges` dedup made real or removed (comment currently
  overclaims); `merge_registry` dedup by actual local names, not prefix;
  hoist the namespace check above the per-proxied-call `rtx`;
  `cache::store` surfaces serialize errors; wrong-type tool args say
  "expected string, got number" instead of "missing"; `--interval`
  overflow clamped; serve's stdout writes tolerate a vanished client
  (no broken-pipe panic); `pub`/`pub(crate)` visibility mismatches fixed;
  `obj_map` clone pass removed; README carries the Tier-3 poll budget.
- Post-cut dead-code sweep: removing printers/bench orphans helpers —
  `cargo clippy` dead-code warnings + a manual pass over `pub(crate)`
  items with no remaining callers.
- `cli.rs` (~1.4k lines) splits: `cli/mod.rs` (clap + dispatch),
  `cli/pull.rs`, `cli/read.rs`, `cli/call.rs`; printing collapses to
  `serde_json::to_string_pretty` at one seam.

## 5. Release binaries (GitHub Releases; no Homebrew)

`release.yml`: on tag `v*` — matrix build `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `x86_64-unknown-linux-gnu` (`cargo build
--release`), strip, tar.gz as `figmog-<version>-<target>.tar.gz`,
generate `SHA256SUMS`, create a **draft** GitHub Release with the
artifacts attached. Publishing the draft is manual (and gated per §3
until fold is licensed). README's install section: download, untar,
`chmod +x`, optionally `xattr -d com.apple.quarantine` note for
unsigned macOS binaries (no codesigning/notarization in v1 — documented
limitation).

## 6. New-repo CLAUDE.md (standing rules)

- Determinism: sorted output at every boundary; no HashMap iteration at
  output boundaries; serde_json never with `preserve_order`.
- On-disk schema: sink names frozen; `Id`/`Rec` enums append-only
  (postcard variant indices).
- fold/bogkit is upstream: never vendored, never patched; consume the
  pinned git dep's public API only.
- Gates for any change: `cargo test`, `cargo clippy --no-deps -- -D
  warnings`, `cargo fmt --check`.
- No new dependencies without a written justification in the PR.
- Fixtures are synthetic only — nothing derived from real client files.

## 7. Sequencing

1. In-flight multi-file serve work completes in the current repo
   (Task 1 done; review running; Task 2 e2e/docs next) — it migrates
   wholesale.
2. Adversarial whole-branch review of the multi-file milestone; push to
   PR #1 (final state of the example-crate era).
3. New repo created (**user confirms creation of
   `sanctuarycomputer/figmog` before any `gh repo create` runs**).
4. Migration executes per this spec: import → git-dep switch → cuts →
   debt payment → module split → SPEC.md consolidation → CI + release
   workflow → tag `v0.1.0` draft release.
5. PR #1 gains a "graduated to sanctuarycomputer/figmog" note; it stays
   open as the hackathon artifact (upstream submission remains a separate,
   user-initiated act from the fork).

## 8. Non-goals

Homebrew tap/formula (deferred); codesigning/notarization; crates.io
publication (fold isn't on crates.io, so figmog can't be);
Windows builds; history-preserving migration.
