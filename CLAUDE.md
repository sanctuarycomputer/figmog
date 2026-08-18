# Working in this repo

figmog is a fold-backed local mirror of one or more Figma files: a CLI
(`pull` + read commands) and an MCP stdio server (`serve`) sharing one
engine. See [`docs/SPEC.md`](docs/SPEC.md) for the current-state spec.
`docs/history/` holds the retired build-design specs and plans
(bench/REPL/`--interactive`/`figmog watch`/human-mode output — all
removed) verbatim, for provenance only — don't treat them as current.

## Standing rules

- **Determinism.** Sorted output at every boundary; no `HashMap`
  iteration ever reaches an output boundary; `serde_json` is never used
  with the `preserve_order` feature.
- **On-disk schema is frozen.** Pipeline sink names (`nodes`, `children`,
  `text`, `instances_of`, `styled_by`, `bound_to`, `by_type`,
  `components`, `component_sets`, `styles`, `variables`,
  `variable_collections`, `meta`, `proxy_cache`, `mirror_config`,
  `images`) are on-disk schema — renaming one changes what store
  directory an unmodified binary can open. `Id`/`Rec` are append-only
  enums: postcard encodes variant indices positionally, so a new variant
  goes at the end of each enum, never inserted or reordered.
- **New record kind, not a wider struct.** Postcard encodes struct fields
  positionally too, so adding a field to an existing `Rec` payload struct
  (`FileMeta`, `NodeRec`, …) breaks decoding of every store already on
  disk, while appending an `Id`/`Rec` variant does not. When a feature
  needs new stored state, add a new record kind (and its own appended
  sink) rather than widening an existing one — `MirrorConfig` is the
  worked example.
- **`fold`/`bogkit` is upstream.** Never vendored, never patched — consume
  only the pinned git dependency's public API.
- **Gates for any change:** `cargo test`, `cargo clippy --no-deps
  --all-targets -- -D warnings`, `cargo fmt --check`, and `cargo doc
  --no-deps` warning-free.
- **No new dependencies without a written justification in the PR.**
- **Fixtures are synthetic only** — nothing derived from real client
  files.
- **License: AGPL-3.0-only, no per-file headers (yet).** figmog's own
  code is AGPL-3.0-only as of v0.0.2; the v0.0.1 snapshot stays MIT.
  Per-file license headers were deliberately skipped. Revisit both that
  decision and a contributor license agreement if the project starts
  taking outside contributions, since relicensing or dual-licensing later
  needs the rights a CLA collects.
- **Pin-bump checklist.** Bumping `fold`'s pinned `rev` in `Cargo.toml`
  isn't just a version bump: `cli/mod.rs`'s `open_store_checked` matches
  the substring `"Locked"` against a caught panic's payload to distinguish
  fjall's lock-contention panic from every other kind (see its doc
  comment). Re-verify that substring still appears in the panic message a
  locked-store `fold`/fjall open actually produces on any pin bump — a
  wording change upstream would silently turn every "store is locked"
  case into `internal panic: ...` + exit 101 instead of the intended
  clean exit-1 message.
- **The empty `[workspace]` table in `Cargo.toml` is deliberate, not
  leftover.** It guards against Cargo silently treating figmog as a
  member of some *enclosing* workspace if this repo is ever checked out
  nested inside another Cargo project — an empty `[workspace]` pins this
  crate as its own workspace root regardless of what's above it.
  Harmless (and currently a no-op) as a standalone repo with nothing
  above it; kept for that scenario anyway, and commented in place in
  `Cargo.toml` itself.
