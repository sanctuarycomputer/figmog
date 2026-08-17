# figmog — a fold-backed local mirror of a Figma file

**Date:** 2026-08-15
**Status:** Draft for review
**Crate:** `examples/figmog`
**Target file (dev/manual testing):** `flAtUnMfzvA5daBSTFQK35` (g3d-Index-Web-Handoff)

## 1. Problem

Figma's MCP server and REST API are slow and aggressively rate limited.
Since the November 2025 rate-limit overhaul, the file endpoints
(`GET /v1/files/:key`, `.../nodes`, `/v1/images`) are Tier 1: **~10
requests/min on the free (Starter) plan**. There is no delta API — the only
way to see what changed is to re-download the entire multi-MB document tree.
Webhooks are unavailable on the free plan, and `FILE_UPDATE` is debounced up
to 30 minutes anyway.

The result: every agent interaction with a Figma file re-pays a huge fetch
through a stingy per-minute bucket.

## 2. Goal

**Lightning-fast local reads of one Figma file for agents.** A sync engine
pulls the file when it changes (spending Tier-1 budget only on actual
changes) and maintains materialized indexes in a fold database. A CLI reads
those indexes locally — point lookups, tree walks, full-text search,
design-system queries — with zero Figma calls and zero rate limits.

Fold's `KeyedStream` upsert semantics synthesize the delta API Figma lacks:
re-upserting a byte-identical record causes **zero graph churn**; a changed
record is retracted from and re-inserted into every index atomically. A
10,000-node file where one layer was renamed costs one fetch plus one
retract/insert pair.

First-class support for Figma's design-system features — components,
component sets, variants and their property axes, styles, bound-variable
references — because a later layer will sync this design system into
Tailwind. That layer is **out of scope** here; this spec only guarantees the
data it needs is mirrored and queryable.

### Non-goals (v1)

- Image renders / thumbnails.
- MCP server in v1 (see §11 for the v2 design: a `serve` subcommand with
  integrated sync — same store, no schema migration).
- Multi-file / team mirroring (the store layout is per-file-key, so this is
  additive later).
- Embeddings / HNSW semantic search (BM25 over names + text is enough for
  v1; an ese branch can be added to the pipeline later; keeping ese out
  keeps builds fast).
- Writing back to Figma.
- Dev Mode extras (annotations, measurements, dev resources, Code
  Connect) — not needed by the Tailwind layer.
- Calling the Enterprise-only Variables REST endpoints. Variables are
  still fully supported — see §6a for how, on a free plan.

## 3. Architecture

Three units with one shared type vocabulary:

```
          ┌─────────────┐   file JSON   ┌──────────────┐  (Id, Rec) recs  ┌───────────────┐
Figma ───▶│  api (HTTP)  │──────────────▶│   flatten    │─────────────────▶│ store (fold)  │
          └─────────────┘               └──────────────┘                  └───────┬───────┘
             ▲    ▲                                                               │ rtx (local)
        pull │    │ watch (poll Tier-3 metadata; Tier-1 fetch only on change)     ▼
          ┌──┴────┴─────────────────────────────────────────────────────────────────────┐
          │                                cli                                          │
          │  pull · watch · tree · get · search · instances · components · styles · …   │
          └─────────────────────────────────────────────────────────────────────────────┘
```

- **`api`** — thin HTTP client behind a `FigmaApi` trait (so every other
  unit is testable without a network). Two calls: `file_meta(key)` (Tier 3,
  cheap, for change detection) and `file(key)` (Tier 1, the full document).
  Honors `Retry-After` on 429.
- **`flatten`** — pure function: file JSON → deterministic
  `Vec<(Id, Rec)>`. All parsing, variant-name parsing, and canonicalization
  lives here. No I/O.
- **`store`** — owns the fold `KeyedStream` and pipeline definition, the
  sync transaction (upsert + remove-vanished + meta bump, all in one `wtx`),
  and typed read helpers.
- **`cli`** — clap-based command surface; every read command supports
  `--json` for agents and a human-readable default.

### Module map

```
examples/figmog/src/
  main.rs      — arg parsing, dispatch, process exit codes
  api.rs       — FigmaApi trait, UreqApi impl, RateLimited/Http errors
  flatten.rs   — file JSON -> Vec<(Id, Rec)>; pure, deterministic
  model.rs     — Id, Rec, NodeRec, ComponentRec, ComponentSetRec, StyleRec,
                 VariableRec, VariableCollectionRec, FileMeta
  store.rs     — pipeline construction, open(), sync(), read helpers
  cli.rs       — subcommand impls, output formatting (human + JSON)
tests/
  flatten.rs   — unit tests over fixtures
  sync.rs      — churn/diff/removal tests over fixtures
  cli.rs       — CLI smoke tests against a fixture-built DB
  fixtures/    — small SYNTHETIC file JSONs (see §9 on provenance)
```

## 4. Data model

One `KeyedStream<Id, Rec>` over one fjall store at `.figmog/<file_key>/`.
Everything the sync writes — nodes, design-system metadata, and the file
meta row — flows through the same stream, so a sync commits **atomically**:
readers never observe a half-applied pull or a version number ahead of its
data.

```rust
enum Id   { Node(String), Component(String), ComponentSet(String), Style(String),
            Variable(String), VariableCollection(String), Meta }
enum Rec  { Node(NodeRec), Component(ComponentRec), ComponentSet(ComponentSetRec),
            Style(StyleRec), Variable(VariableRec),
            VariableCollection(VariableCollectionRec), Meta(FileMeta) }
```

`NodeRec` (one per node in the document tree, keyed by Figma's stable node
id):

| field | type | notes |
|---|---|---|
| `id` | `String` | Figma node id, e.g. `"12:34"` |
| `parent_id` | `Option<String>` | `None` only for the document root |
| `child_index` | `u32` | position within parent |
| `page_id` | `String` | enclosing CANVAS id (root/pages: own id) |
| `node_type` | `String` | `FRAME`, `TEXT`, `COMPONENT`, `COMPONENT_SET`, `INSTANCE`, … |
| `name` | `String` | layer name |
| `visible` | `bool` | absent in JSON ⇒ `true` |
| `text` | `Option<String>` | `characters` for TEXT nodes |
| `component_id` | `Option<String>` | INSTANCE → its component's node id |
| `component_properties` | `Vec<(String, String)>` | INSTANCE `componentProperties` assignments (variant values, booleans, text, instance swaps), **sorted by key**; values stringified |
| `property_definitions` | `Option<String>` | `componentPropertyDefinitions` as canonical JSON — present on **both** COMPONENT and COMPONENT_SET nodes (property types: VARIANT, BOOLEAN, TEXT, INSTANCE_SWAP) |
| `style_refs` | `Vec<(String, String)>` | node `styles` map as (style_type, style_id), **sorted** — style types FILL, TEXT, EFFECT, GRID |
| `bound_variables` | `Vec<(String, String)>` | every variable binding in this node's JSON as (json_path, variable_id), **sorted** — collected by a generic recursive scan for `boundVariables` objects (robust to Figma adding bindable properties), not per-property typed extraction |
| `abs_bounds` | `Option<[f64; 4]>` | absoluteBoundingBox x, y, w, h |
| `raw` | `String` | canonical JSON of the node **with `children` stripped** — full fidelity for `get` |

`ComponentRec` / `ComponentSetRec` (from the file response's `components` /
`componentSets` maps, keyed by node id): `node_id`, `key` (global key),
`name`, `description`, `remote` (library component used by an instance vs
defined locally), and for components `component_set_id: Option<String>`.
`StyleRec` (from `styles`, keyed by style id): `style_id`, `key`, `name`,
`style_type`, `description`, `remote`. `FileMeta`: `name`, `version`,
`last_modified`, `synced_at_unix_ms` — `last_modified` is the *file*
endpoint's (`GET /v1/files/:key`) `lastModified` field, populated on every
`pull`; see §6 for how this compares against the *meta* endpoint's
`last_touched_at` during `watch`.

`VariableRec` / `VariableCollectionRec` (populated by `import-variables`
only — see §6a): variable `id`, `name`, `resolved_type`
(COLOR/FLOAT/STRING/BOOLEAN), `collection_id`, `values_by_mode` as
canonical JSON (values or `VARIABLE_ALIAS` refs), `description`,
`scopes`; collection `id`, `name`, `modes` as sorted (mode_id, mode_name)
pairs, `default_mode_id`.

Variant support falls out of this model: a COMPONENT_SET node's variants are
its COMPONENT children (the `children` index gives the axis), the set's
`property_definitions` carries the axes/options, and each INSTANCE's
`component_properties` carries its chosen variant values. `instances_of`
answers "every usage of this component"; joining through
`ComponentRec.component_set_id` answers "every usage of any variant of this
set".

### Determinism (byte-equality contract)

`KeyedStream::upsert` detects change by comparing postcard bytes, so
`flatten` must be a pure, deterministic function of the file JSON:

- Every map-shaped field is stored as a **sorted `Vec` of pairs**, never a
  `HashMap`.
- `raw` / `property_definitions` are re-serialized through `serde_json`
  **without** the `preserve_order` feature, so object keys are
  BTreeMap-sorted and canonical.
- No timestamps, randomness, or environment reads inside `flatten`.
  (`FileMeta.synced_at_unix_ms` is the one wall-clock value and lives only
  in the meta row, which is expected to change every sync.)

## 5. Pipeline

```rust
KeyedStream<Id, Rec>::new(db_path, (
    // -- node branch: FilterMap Keyed<Id,Rec> -> Keyed<String, NodeRec> --
    FilterMap(node_only, (
        terminal::Table::new("nodes"),                       // id -> NodeRec
        Map(|n| Keyed::new(n.parent_id?, (n.child_index, n.id)),
            terminal::Multimap::new("children")),            // parent -> (idx, child)
        FilterMap(|n| non_empty(name + text),
            terminal::search::Bm25::new("text")),            // full-text
        FilterMap(|n| n.component_id.map(|c| Keyed::new(n.id, c)),
            terminal::InvertedIndex::new("instances_of")),   // component id -> instances
        FlatMap(|n| n.style_refs -> Keyed::new(n.id, style_id),
            terminal::InvertedIndex::new("styled_by")),      // style id -> nodes
        FlatMap(|n| n.bound_variables -> Keyed::new(n.id, variable_id),
            terminal::InvertedIndex::new("bound_to")),       // variable id -> nodes
        Map(|n| Keyed::new(n.id, n.node_type),
            terminal::InvertedIndex::new("by_type")),        // type -> nodes
    )),
    // -- design-system branch --
    FilterMap(component_only,     terminal::Table::new("components")),
    FilterMap(component_set_only, terminal::Table::new("component_sets")),
    FilterMap(style_only,         terminal::Table::new("styles")),
    FilterMap(variable_only,      terminal::Table::new("variables")),
    FilterMap(collection_only,    terminal::Table::new("variable_collections")),
    // -- meta branch --
    FilterMap(meta_only,          terminal::Table::new("meta")),
))
```

(Pseudocode: real code uses fold's `FilterMap::new(closure, next)` forms.
The point is the shape: one stream, typed branches, seven node sinks.)

Notes:

- `children` values sort by postcard encoding, which is **not** numeric
  order for varint-encoded `u32` — readers sort the returned `Vec` by
  `child_index` before output. Alternatively `child_index` is stored
  big-endian-encoded; decision left to implementation, but output order
  must be numeric.
- All closures are pure functions of the record, as fold requires for
  retraction to cancel.
- Sink names are frozen here; they are part of the on-disk schema. Any
  rename is a breaking change requiring a re-pull (acceptable v1 policy:
  `figmog pull --fresh` wipes and rebuilds).

## 6. Sync engine

**`pull`** — one full refresh, one `wtx`:

1. `api.file(key)` — single Tier-1 request. No `geometry=paths` (vector
   outlines bloat the payload and serve no v1 query).
2. `flatten` → `Vec<(Id, Rec)>` + the set of live `Id`s.
3. Read the currently stored id set (snapshot read of the nodes,
   components, component_sets, and styles tables). **Variable and
   collection records are exempt from the sweep** — they come from
   `import-variables`, not the file fetch, and must survive pulls.
4. In one `wtx`: `upsert` every flattened record (unchanged records
   short-circuit inside fold), `remove` every stored sweepable id not in
   the live set, upsert `FileMeta`.
5. Print a churn summary (added / changed / removed counts — obtained by
   counting `upsert`/`remove` return values, which report the prior
   record).

**`watch`** — the polling loop:

```
loop:
  meta = api.file_meta(key)            # Tier 3: 50/min budget on Starter
  if meta.last_touched_at != stored:   # documented as content-modification time
      pull()
  sleep(interval)                      # default 10s, --interval to change
```

- A spurious trigger (touch without a real edit) costs one Tier-1 fetch
  that produces zero churn — the design is self-healing, so the trigger
  needs to be cheap, not perfect.
- The comparison above is across endpoints: `meta.last_touched_at` comes
  from `GET /v1/files/:key/meta` (Tier 3), while the stored watermark
  (`FileMeta.last_modified`) was captured from `GET /v1/files/:key`'s
  `lastModified` field (Tier 1) on the last `pull`. If the two fields ever
  differ in format or precision, every `watch` start on a warm DB costs one
  spurious Tier-1 pull that produces zero churn — self-healing within the
  run, since the Watcher then keeps `last_touched_at` in memory and stops
  re-triggering. See §9's manual live check for confirming the two fields
  agree on a real file.
- On 429: sleep `Retry-After` seconds, then resume (single-process poller;
  no jitter). On network errors: exponential backoff capped at 5 min, keep
  looping — `watch` must survive laptop sleep and flaky wifi. The same
  discipline applies if the Tier-1 pull itself fails after a detected
  change (429 → `Retry-After`; anything else → the same exponential
  backoff), so a persistently failing pull doesn't hammer the Tier-1
  budget.
- `watch` performs an initial `pull` if the DB is empty or stale.

Auth: personal access token from `FIGMA_TOKEN` (flag `--token` overrides).
File identity: accept a bare key, a full `figma.com/design/...` URL, or a
URL with `?node-id=`; node ids accept both `12:34` and `12-34` forms
everywhere.

### 6a. Variables & design tokens on a free plan

The Variables REST endpoints (`/v1/files/:key/variables/local`, `/published`)
are **Enterprise-only**, so figmog never calls them. Variables are supported
through two complementary paths:

**Path 1 — mirrored bindings + inference (always on, zero setup).** The
file JSON annotates every variable-bound property with a `boundVariables`
object, and Figma also bakes the *resolved* concrete value into the same
node (the fill's color, the padding's number, …). Flatten collects every
binding as `(json_path, variable_id)` on `NodeRec` (generic recursive
scan), and the `bound_to` index inverts them. `figmog vars` then
aggregates at read time: for each variable id — its binding sites
(node, property path) and the *observed values* extracted from the
consumers' `raw` JSON next to each binding. This yields
`variable id → inferred value(s) + everywhere it's used`, which covers the
default mode (values from non-default modes appear only where a frame
explicitly overrides the mode — documented caveat).

**Path 2 — authoritative import (optional).** `figmog import-variables
<export.json>` upserts `VariableRec` / `VariableCollectionRec` — full
fidelity: collections, modes (light/dark), per-mode values, aliases,
scopes. Accepted shapes: the Enterprise REST `variables/local` response,
or the JSON produced by the standard free-plan escape hatch — a Figma
plugin export (the Plugin API can read local variables on **any** plan;
figmog's README ships a ~20-line "run in Figma's plugin console" snippet
that dumps the REST-shaped JSON). Imports flow through the same
`KeyedStream`, so re-imports diff incrementally like everything else and
`figmog vars` prefers authoritative records over inference when present.

A third source exists for paid seats only, noted for completeness and
deliberately **not** built in v1: Figma's MCP servers expose
`get_variable_defs`, but the desktop server needs a Dev/Full seat on a
paid plan, the remote server allows Starter users only 6 tool calls per
*month*, and the tool is selection-scoped rather than
full-collections. Anyone with a paid seat can pipe its output into
`import-variables` by hand; figmog never depends on MCP.

Everything else the Tailwind layer needs is already mirrored at full
fidelity in `raw` and queryable through the indexes: TypeStyle (font
family/size/weight/line-height/letter-spacing), fills/strokes/effects,
auto-layout (layoutMode, padding, itemSpacing, sizing), corner radii,
grids, `characterStyleOverrides`/`styleOverrideTable` for mixed text, and
component property definitions. Style *definitions* are not in the file
JSON (the `styles` map is metadata only), so `figmog styles --values`
derives each style's value from its `styled_by` consumers — e.g. a text
style's TypeStyle from any TEXT node using it.

## 7. CLI surface

Read commands never touch the network. All output ordering is
deterministic (sorted); `--json` emits machine-readable JSON on stdout.

| command | reads | behavior |
|---|---|---|
| `figmog pull <file>` | — | sync now; prints churn summary |
| `figmog watch <file> [--interval 10]` | — | poll loop as above |
| `figmog pages` | children of root | list CANVAS pages (id, name) |
| `figmog tree [id] [--depth N]` | children + nodes | indented outline: `name  [type]  id`; root defaults to document |
| `figmog get <id> [--children]` | nodes (+children) | the full `raw` JSON of a node; `--children` inlines one level of child summaries |
| `figmog search <query> [-n 10]` | Bm25 + nodes | ranked hits: score, id, type, name, page, text snippet |
| `figmog instances <id\|key\|name>` | components + instances_of | resolve arg to a component (by node id, global key, or unique name / name of a set ⇒ all its variants), list instance nodes |
| `figmog components` | components + component_sets + children | design-system inventory: sets with their variant axes/options, standalone components |
| `figmog styles [--type text\|fill\|effect\|grid] [--values]` | styles + styled_by (+nodes) | styles with usage counts; `--values` derives each style's definition from consumer nodes (§6a) |
| `figmog uses <style_id\|variable_id>` | styled_by / bound_to + nodes | nodes using a style or bound to a variable |
| `figmog vars [id]` | bound_to + nodes + variables | variables: authoritative records if imported, else inferred values + binding sites (§6a) |
| `figmog import-variables <export.json>` | — | upsert variable/collection records (§6a Path 2) |
| `figmog find --type TEXT [--page id]` | by_type + nodes | nodes by type, optional page filter |
| `figmog status` | meta | file name, version, last modified, last synced, node count |

DB location: `.figmog/<file_key>/` under the current directory (override
`--db`). The CLI stores the last-used file key in `.figmog/current` so read
commands don't need the file argument every time.

## 8. Rust practices

- **Errors:** `thiserror` error enums per module; no `unwrap`/`expect` on
  network, parse, or user-input paths. `unwrap` is acceptable only where
  fold itself panics by contract (store open, duplicate sink names) and in
  tests. CLI exits nonzero with a one-line message on error; `--json` mode
  errors are JSON on stderr.
- **Testability by construction:** `FigmaApi` is a trait; `flatten` is
  pure; `store::sync` takes pre-flattened records. The `watch` loop takes
  its sleep function as a parameter. No test needs a network or a real
  clock.
- **Typed edges, dynamic core:** the response envelope (`name`, `version`,
  `components`, `styles`, …) parses into serde structs; node trees parse as
  `serde_json::Value` (the node schema is huge and mostly passed through
  `raw`), with typed extraction only for the `NodeRec` fields. Exact field
  names pinned against `figma/rest-api-spec` (OpenAPI) during
  implementation.
- **Determinism everywhere:** no `HashMap` iteration at any output
  boundary (matches the repo's standing rule); sorted `Vec`s in records;
  canonical JSON; CLI output sorted.
- **Hygiene:** `cargo clippy -- -D warnings` and `cargo fmt --check`
  clean; rustdoc on every public item; crate-level doc comment explaining
  the pipeline (matching the style of the `search` example).
- Workspace: scaffolded via `./scripts/new-project.sh figmog`; deps
  `fold`, `serde`, `serde_json`, `postcard` (transitively via fold),
  `ureq`, `clap`, `thiserror`. **No `ese`** (build speed), no tokio (ureq
  is blocking; the watch loop is a plain thread).

## 9. Test plan

Fixtures are **synthetic** miniature Figma file JSONs (hand-written,
~30–60 nodes) exercising: 3 pages, nested frames, TEXT nodes, a
COMPONENT_SET with two axes (e.g. `Size` × `State`), variant COMPONENT
children, INSTANCEs with `componentProperties` covering all four property
types, standalone COMPONENT with non-variant properties, fill/text style
refs, `boundVariables` refs at several depths, an invisible node — plus a
separate REST-shaped variables-export fixture for `import-variables`. **No
fixture may be derived from the g3d file** — it is client work and stays
out of git (same policy as the repo's garden3d-corpus rule). The real file
is used only for local manual verification.

1. **Flatten unit tests** (`tests/flatten.rs`)
   - ids/parents/child_index/page attribution correct for the whole fixture
   - TEXT `characters` extracted; visibility default handling
   - INSTANCE → `component_id` + sorted `component_properties` (all four
     property types: VARIANT, BOOLEAN, TEXT, INSTANCE_SWAP)
   - `property_definitions` canonical JSON on both COMPONENT and
     COMPONENT_SET nodes
   - style refs sorted and complete
   - bound-variable scan finds bindings at every depth (a fill, a
     TypeStyle field, a padding, a deeply nested property) and is
     unaffected by unknown/new binding sites
   - `raw` has no `children` key; parses back to JSON
   - **Determinism:** flatten the same JSON twice (and a key-order-shuffled
     copy of it) → identical postcard bytes per record.
2. **Sync tests** (`tests/sync.rs`) — pipeline built with a test-only
   delta-probe (a passthrough `Map` incrementing a `Rc<Cell<usize>>`)
   between stream and sinks:
   - **No-churn:** pull fixture, pull identical fixture again → probe count
     0 in the second `wtx`; all sink contents identical.
   - **Minimal-churn diff:** fixture v1 → v2 (one rename, one node
     deleted, one added, one instance's variant property changed) → probe
     count equals exactly the expected retract+insert pairs; Bm25 no
     longer matches the old name but matches the new one; deleted node
     absent from `nodes`, `children`, `by_type`; new node present
     everywhere applicable.
   - **Removal cascade:** delete an INSTANCE → it disappears from
     `instances_of`; delete a styled node → `styled_by` count drops.
   - **Vanished-id sweep:** node present in store but absent from fetch is
     removed even when nothing else changed.
   - **Atomicity:** a flatten record that panics mid-`wtx` (injected) leaves
     the store at the previous version (meta unchanged, old data readable).
3. **Watch-loop tests** (unit, fake `FigmaApi` + recorded sleeps)
   - unchanged metadata → no `file()` call
   - changed metadata → exactly one `file()` call, then quiescent
   - 429 with `Retry-After: 30` → recorded sleep ≥ 30s, loop continues
   - network error → backoff grows, caps, loop continues
4. **Design-token tests**
   - `vars` inference: fixture with a color variable bound to fills on two
     nodes → inferred value equals the baked-in color, both binding sites
     listed
   - `import-variables` round trip: import a REST-shaped export (two
     collections, two modes, an alias) → `vars` shows authoritative
     values incl. per-mode; re-import of identical file → zero churn
     (delta probe); import with one changed mode value → minimal churn
   - `styles --values`: text style value derived from a consumer TEXT
     node's TypeStyle; fill style from a consumer's fills
5. **CLI smoke tests** (`tests/cli.rs`, via `assert_cmd` or equivalent)
   - build a DB from the fixture, run each read command, snapshot-assert
     stdout (`--json` mode: parse and assert structurally)
   - URL/key/node-id argument parsing (`12-34` ⇒ `12:34`, full URLs)
6. **Manual live check** (documented in the crate README, not CI): `FIGMA_TOKEN=… figmog pull <g3d url>`, then `figmog components`,
   `figmog search`, timing note. Acceptance: read commands return in
   milliseconds on the real file. Also confirm `last_touched_at` (meta) equals
   `lastModified` (file) on the real file, since `watch` compares them
   across two different endpoints (§6).

Full-feature test run (`cargo test -p figmog`) must pass before the
milestone is called done; `-p figmog` doesn't build ese, so iteration is
already fast.

## 10. Risks & open items

- **Change-detection field pinned:** `GET /v1/files/:key/meta` returns
  `last_touched_at`, documented in the OpenAPI spec as "the UTC ISO 8601
  time at which the file content was last modified". Fallback if it
  proves noisy: the `versions` endpoint (Tier 2).
- **File size:** GET file for a large handoff file can be tens of MB;
  `ureq` reads it streaming into `serde_json`. If flatten+parse of the
  real file is slow (>2–3s), acceptable — it's off the read path.
- **Instance sub-tree contents:** Figma serializes instances' overridden
  subtrees as normal children; they mirror like any node. Overrides beyond
  the serialized tree are not resolved (documented).
- **Branching files:** `branch_data` ignored in v1; mirroring a branch =
  mirroring its own file key.

## 11. v2: `figmog serve` — the MCP server

The point of the whole project: agents talk MCP, so the mirror gets an MCP
face. **Revision of the v1 non-goal sketch:** this is a `serve` subcommand
of the same binary, not a second binary — fjall is single-writer, so a
standalone MCP process would fight `figmog watch` for the store lock.
`figmog serve` is therefore **one process that owns the store**: an MCP
stdio server with the sync loop integrated. Agents get always-fresh reads;
there is nothing else to run.

### Architecture

```
stdin ──▶ reader thread ──▶ mpsc<String> ──▶ main loop ──▶ stdout (responses)
                                              │  recv_timeout(next poll tick)
                                              ├─ on line:    JSON-RPC dispatch → query::*
                                              └─ on timeout: Watcher::tick → maybe pull
```

- **`query.rs` (refactor):** the read logic currently inlined in the CLI's
  `cmd_*` printers moves into pure functions that take readers and return
  `serde_json::Value` — `query::status`, `query::pages`, `query::tree`,
  `query::node`, `query::find`, `query::search`, `query::instances`,
  `query::components`, `query::styles`, `query::uses`, `query::vars`. The
  CLI commands become thin printers over `query::*` (this also retires the
  deferred "json/human boilerplate" debt); MCP tools call the same
  functions. One source of truth for every answer.
- **`mcp.rs`:** minimal JSON-RPC 2.0 over newline-delimited stdio. Handles
  `initialize` (echo the client's `protocolVersion`; `capabilities:
  {tools: {}}`; `serverInfo {name: "figmog", version}`),
  `notifications/initialized` (ignore), `ping`, `tools/list`,
  `tools/call`. Everything else → JSON-RPC `-32601`. Malformed JSON →
  `-32700` with `id: null`. Logging to stderr only; stdout carries nothing
  but protocol frames.
- **`serve.rs`:** the loop above. Store owned by the main thread (no
  `Send` requirements on fold types). Poll ticks run only between
  requests; a pull blocks request handling for its duration (documented —
  seconds at worst, and only when the file actually changed).

### Relationship to Figma's official MCP server (binding; revised for v3)

**Positioning (v3 decision): figmog is the ONLY Figma MCP an agent
connects to** — a cached proxy in front of Figma's native desktop server,
plus the local mirror's own query tools. Within the one server, the
namespace rule keeps every call unambiguous:

1. **Native-named tools are always proxied.** Tools discovered from the
   upstream desktop server (`get_design_context`, `get_screenshot`,
   `get_metadata`, `get_variable_defs`, …) are re-exposed verbatim and
   answered by the upstream (through the cache) with native semantics and
   output formats — figmog never impersonates them with its own data.
2. **`figmog_*` tools are always local.** The mirror's query tools answer
   instantly from the store at zero API cost.
3. **Server-level steering:** the `initialize` result's `instructions`
   field carries, verbatim: "figmog is your Figma server: a local,
   instant mirror of one Figma file plus a cached proxy to Figma's native
   capabilities. Call figmog for everything Figma-related. figmog_* tools
   answer from the local mirror at zero API cost; native-named tools
   (get_*, …) go to Figma, cached by file version where possible."

This targets **paid Dev/Full seats** (the desktop server's requirement).
The free-plan-only paths (plugin-console variables export, inference as
primary) remain in the code as fallbacks but are no longer the design
center.

### Tools

Read tools mirror the CLI one-to-one, each returning the `query::*` JSON
as an MCP text content block. Names and inputs:

| tool | input schema (all fields optional unless noted) |
|---|---|
| `figmog_status` | — |
| `figmog_pages` | — |
| `figmog_tree` | `id`, `depth` (integer) |
| `figmog_node` | `id` (required), `children` (bool) |
| `figmog_find` | `type` (required), `page` |
| `figmog_search` | `query` (required), `limit` (integer, default 10) |
| `figmog_instances` | `target` (required) |
| `figmog_components` | — |
| `figmog_styles` | `type`, `values` (bool) |
| `figmog_uses` | `id` (required) |
| `figmog_vars` | `id` |
| `figmog_sync` | — (forces one pull; returns churn; the only tool that spends rate budget) |

**Whole-file structural queries** (the local mirror's unfair advantage —
each is a full-file answer no rate-limited API surface could offer; all
are read-only scans/joins over existing sinks, and each gets a matching
CLI subcommand so the CLI/tool one-to-one rule holds):

| tool / CLI command | input | answer |
|---|---|---|
| `figmog_stats` / `figmog stats` | — | node counts by type and by page, component/set/style/variable totals, text-node count, max tree depth |
| `figmog_path` / `figmog path <id>` | `id` (required) | ancestor chain root→node as `[{id, name, type}]` |
| `figmog_text` / `figmog text [--page id]` | `page` | every TEXT node's (id, characters, page_id), sorted by id |
| `figmog_where` / `figmog where --pointer /p --equals <json>` | `pointer` (required, RFC 6901 into node `raw`), `equals` (JSON value; omitted ⇒ "pointer exists"), `page` | matching `[{id, name, type, page_id, value}]`, sorted by id |
| `figmog_at` / `figmog at --x N --y N` | `x`, `y` (required, floats) | nodes whose `abs_bounds` contain the point, sorted by area ascending (deepest/smallest first) |

`figmog_where`'s `equals` compares the pointed-at value by JSON equality
(numbers per serde_json semantics). Full-node scans are acceptable: they
run against the local store, not Figma.

Tool-level failures (unknown node, no mirror, sync error) return an MCP
result with `isError: true` and the message as text — JSON-RPC errors are
reserved for protocol-level problems.

### CLI surface

`figmog serve [file] [--interval N] [--no-watch]` — `--no-watch` disables
the poll loop (offline/fixture use; also what tests run). File/db
resolution identical to the other commands.

### Testing

- **Protocol unit tests** (`mcp.rs`): dispatch table over scripted
  request values — initialize echo (incl. the steering `instructions`),
  tools/list shape (17 tools, valid JSON-Schema inputs), unknown method
  `-32601`, parse error `-32700`, tools/call routing incl. `isError` on a
  bad tool name.
- **`query` equivalence:** the CLI smoke tests keep passing unchanged
  after the refactor (the printers now consume `query::*`), proving the
  refactor moved logic without changing it.
- **End-to-end serve test** (`tests/serve.rs`): build a fixture DB via
  `pull --from-file`, spawn `figmog serve --no-watch --db …` as a child
  process, drive initialize → tools/list → several tools/call over
  stdin/stdout, assert JSON-RPC ids, tool result contents (e.g.
  `figma_search` finds node 1:2), and `isError` for an unknown node.
- **Live check addendum:** point Claude Code at the server
  (`claude mcp add figmog -- <path>/figmog serve <url>`) and ask it about
  the file.

### Non-goals (v2)

MCP resources/prompts capabilities; HTTP/SSE transports for *our* server;
multi-file serving; auth on the socket (stdio only, inherits process
trust).

## 12. v3: the cached proxy

figmog becomes the only Figma MCP an agent sees: local `figmog_*` tools
plus a verbatim passthrough of the native desktop server's tools, with a
version-keyed response cache. Targets paid Dev/Full seats; requires the
Figma desktop app's Dev Mode MCP server (streamable HTTP at
`http://127.0.0.1:3845/mcp` by default).

### Upstream client (`upstream.rs`)

- `trait UpstreamMcp { fn initialize(&mut self) -> Result<(), UpstreamError>; fn tools(&self) -> &[Value]; fn call(&mut self, name: &str, args: &Value) -> Result<Value, UpstreamError>; }`
  plus `HttpUpstream` (ureq POST of JSON-RPC frames; accept both
  `application/json` bodies and single-event `text/event-stream`
  responses, extracting the `data:` JSON; carry the
  `Mcp-Session-Id` header if the server issues one) and a scripted fake
  for tests. `--upstream <url>` overrides the default;
  `--no-upstream` disables proxying entirely.
- Startup: probe + MCP handshake; on failure, serve local tools only,
  log one stderr line, and report `upstream: "unreachable"` in
  `figmog_status`. No mid-session re-probe in v3 (restart to attach —
  documented).

### Registry merge

`tools/list` = the 17 local `figmog_*` tools followed by every upstream
tool verbatim (name, description, inputSchema passed through; description
prefixed "[via Figma desktop] "). Name collisions are impossible by the
namespace rule; if an upstream tool ever arrives named `figmog_*`, drop
it and log. `tools/call` routes by name: local registry first, else
upstream.

### Cache

- New record kind: `Id::ProxyCache(String /*key hash*/)`,
  `Rec::ProxyCache { key_hash, tool, args_canonical, file_version,
  content: String /*canonical JSON of the MCP result content*/ }`, stored
  through the same stream into a `proxy_cache` Table sink.
- **Cacheable** = tool name starts `get_` or `list_` AND the arguments
  contain an explicit node id (selection-based calls are invisible to the
  cache and always forwarded). Key = hash(tool + canonical args); a hit
  requires `file_version == current FileMeta.version`.
- **Eviction:** during `sync`, when the file version changes, stale
  `ProxyCache` rows (any whose `file_version` differs from the incoming
  version) join the sweep. Manually imported variables remain
  sweep-exempt; the cache is not.
- **Writes:** any non-cacheable upstream call that is not `get_`/`list_`
  (e.g. `add_code_connect_map`, `send_code_connect_mappings`) is
  forwarded uncached and, on success, triggers an immediate meta poll so
  upstream-originated edits reach the mirror without waiting for the
  next tick.

### CLI parity (1:1 via mechanism)

The engine exposes everything; the CLI can invoke anything:
- `figmog tools` — the merged tool list (local + upstream, with source
  and cacheability flags).
- `figmog call <tool> [--args '<json>']` — invoke any tool by name
  through the same dispatch the MCP server uses (local tools included).
Bespoke subcommands for upstream tools are deliberately NOT added —
Figma's tool list churns; the generic mechanism is the stable 1:1
surface.

### Enterprise variables (opportunistic)

`pull` additionally calls `GET /v1/files/:key/variables/local` (Tier 2/
Enterprise): on success its records flow through
`parse_variables_export` into the same sync **and become sweepable for
that pull** (API-provided variables are file state); on 403/404 the call
is skipped silently and the v1 behavior (import/inference, sweep-exempt)
holds unchanged.

### Testing

- Upstream client unit tests against a scripted fake; one in-process HTTP
  fake (std `TcpListener` serving canned JSON-RPC responses, no new
  deps) exercising `HttpUpstream` end to end incl. the SSE-style body.
- Registry merge + routing + cache hit/miss/eviction unit tests (fake
  upstream, fixture store; assert the second identical `get_*` call with
  a nodeId never reaches the fake, and a version bump evicts).
- e2e: serve with `--no-upstream` keeps the v2 behavior (existing tests);
  one e2e with the in-process HTTP fake upstream asserts a proxied tool
  appears in tools/list and round-trips.

### Non-goals (v3)

Proxying the remote server (OAuth); mid-session upstream re-attach /
`listChanged` notifications; caching selection-based calls; multi-file.

## 13. `figmog bench` — the load-test demo

One self-contained command that makes the value proposition measurable:
local reads at memory speed against a rate-limited API that allows ~10
file requests per minute.

`figmog bench [FILE] [--nodes N] [--calls M] [--api-calls K] [--skip-api]
[--json] [--keep]`
(defaults: N=10000, M=5000, K=5; `--keep` leaves the temp store on disk
and prints its path).

**Two sources.** With no `FILE` argument, the corpus is synthetic
(deterministic, phase 1 below). With a Figma URL/key, the corpus is the
real file: fetched once via `FIGMA_TOKEN` (exactly one Tier-1 call — the
no-churn re-pull phase reuses the same in-memory JSON rather than
fetching twice), and the load-test query mix derives its parameters from
the flattened data itself: search words sampled from real layer
names/text, node ids from real ids, the instances target from a real
component name (each falling back gracefully when a category is absent).
The same derivation runs in synthetic mode, so the two modes share one
code path.

**API comparison phase** (real-file mode only, unless `--skip-api`):
after the serve load, issue K sequential calls to
`GET /v1/files/:key/nodes?ids=<real id>` — the native API's closest
equivalent of `figmog_node` — timing each, plus one Tier-3
`GET /v1/files/:key/meta` for reference. K defaults to 5 because this
spends the user's real Tier-1 budget (~10/min); a 429 is recorded (with
its Retry-After) and ends the phase gracefully, reporting whatever was
measured. The report then shows figmog vs API latency side by side and
computes the budget math: how long the M-call load test would take at
the API's rate limit versus figmog's measured wall time.

### Phases (all timed, all reported)

1. **Corpus** — generate a deterministic synthetic Figma file JSON with N
   nodes: pages of auto-layout frames, TEXT nodes whose characters are
   drawn from a fixed word list (so BM25 has real queries), one
   COMPONENT_SET with variants plus INSTANCE nodes referencing them,
   fill/text styles, and `boundVariables` bindings. Determinism: a seeded
   LCG in the generator, no wall-clock, no `rand` dep — the same `--nodes`
   always yields byte-identical JSON. Generator lives in the crate
   (`src/bench.rs` or a `corpus` module) and is unit-testable
   (node count exact, determinism byte-checked).
2. **Cold sync** — flatten + `sync` the corpus into a temp store via the
   library (not a child process): report flatten ms, sync ms, records/s.
3. **No-churn re-pull** — sync the identical corpus again: report ms and
   assert-in-code churn is zero (the engine's headline invariant, timed).
4. **Serve load** — spawn `current_exe()` as
   `serve --no-upstream --no-watch --db <tmp>`, complete the MCP
   handshake, then issue M tools/call frames in a fixed rotating mix
   (`figmog_search` with rotating corpus words, `figmog_node`,
   `figmog_where`, `figmog_stats`, `figmog_tree` (depth 2),
   `figmog_instances`), measuring wall time per request (write→response
   line). Sequential over one stdio pipe — that matches the server's
   single-threaded loop, so the numbers are honest.

### Report

Per-tool table: calls, p50 / p95 / p99 / max (ms), plus overall
sustained req/s and total wall time. In real-file mode with the API
phase: an additional side-by-side block — `figmog_node p50` vs
`API /nodes p50`, the speedup factor, and the budget line ("the M-call
load test at ~10 req/min would take ≈X; figmog: Ts"). `--json` emits one
JSON object with the same fields (stdout purity as elsewhere: human
table OR json, never both). Ends with the headline comparison in both
modes.

### Constraints

No new dependencies. Percentiles via sort. `Instant`-based timing only
(no SystemTime in the measurement path). Synthetic mode must not touch
the network at all; real-file mode makes exactly 1 Tier-1 file fetch, an
opportunistic variables call, and (unless `--skip-api`) K+1 comparison
calls — the report states every API call it spent. Temp dir cleaned
unless `--keep`. Exit nonzero if any phase fails or any tool call
returns `isError` (a graceful 429 in the comparison phase is a recorded
result, not a failure).

### Interactive mode (`--interactive`)

`figmog bench [FILE] --interactive` runs the same setup (corpus or real
file → cold sync → spawn serve child) and then, instead of the automated
phases, drops into a REPL on the user's terminal so requests are visible
as they fire:

- **Tool shorthands** mapping to the local tools with light arg parsing:
  `search <words…>`, `node <id> [children]`, `tree [id] [depth]`,
  `find <TYPE> [page]`, `where <pointer> [value]`, `stats`, `path <id>`,
  `text [page]`, `at <x> <y>`, `instances <target>`, `components`,
  `styles [type]`, `uses <id>`, `vars [id]`, `pages`, `status`. Each
  prints one aligned line: sequence number, tool, arg summary, latency in
  ms, and (dim) a one-line result digest (hit count / name / isError).
- **`run N`** — fire N requests of the derived mixed workload, streaming
  one line per request in real time, then print the session percentile
  table for the burst.
- **`api node <id>` / `api meta`** — real-file mode only: fire one actual
  Figma API call (`/nodes` or `/meta`), timed the same way, each line
  labeled with the API cost it spent. The live side-by-side is the demo's
  centerpiece; 429s print their Retry-After and do not exit.
- **`call <tool> <json-args>`** — raw escape hatch (works for proxied
  tools too when an upstream is attached).
- **`report`** — cumulative per-tool percentiles for everything fired
  this session; **`help`**; **`quit`**/EOF exits cleanly (child reaped).

Colors: raw ANSI escapes only (no deps), emitted only when stdout is a
terminal (`IsTerminal`); latency lines green under 10ms, yellow under
100ms, red above; errors red. Non-TTY stdout gets plain text. The
interactive mode is human-only: `--json` combined with `--interactive`
is a usage error. The one-shot mode is unchanged (CI/e2e cover it);
interactive gets a scripted e2e (commands piped via stdin, non-TTY plain
output asserted, clean exit on EOF).

### Non-goals

Concurrent client simulation (stdio is one pipe; the server is
single-threaded by design); benchmarking the proxy path (network-bound,
not ours to measure); measuring Figma's rate limit itself (the
comparison phase measures API *latency* with K small calls; the
~10/min budget number is documented, never probed to exhaustion);
readline niceties (history/completion — plain stdin lines are enough
for a demo REPL).

## 14. v4: multi-file serve

Agents address Figma by URL — a server bound to one file at startup
breaks that habit. v4 makes `figmog serve` a multi-file server; the
per-file-key store layout (`.figmog/<key>/db`) has anticipated this
since v1.

### Surface

- `figmog serve [FILE]...` — zero or more files at startup. Zero is
  valid: the server starts empty and mirrors files as agents reference
  them. Each startup FILE is pulled if its store is empty.
- **Every local tool gains an optional `file` argument** (URL or key,
  parsed with `ident::parse_file_ref`). Resolution: explicit `file` arg →
  that mirror (auto-opening it if unknown, which spends one Tier-1
  pull); omitted → the default file (first startup FILE, else the single
  mirrored file, else an isError naming `figmog_files`/`figmog_open`).
- New tools: `figmog_open {file}` — mirror a file now (one Tier-1 pull;
  returns churn + node count), and `figmog_files` — list mirrored files
  (key, name, version, nodes, last synced, default flag). Local tool
  count becomes 19.
- The steering `instructions` text is extended with one sentence: "Pass
  the Figma file URL as the `file` argument when you have one; figmog
  mirrors files on first reference."

### Mechanics

- **Sessions:** each mirrored file is a `FileSession` whose store lives
  captured inside boxed closures (`dispatch(tool, args)`,
  `pull()`, `watermark()`) — the established answer to the unnameable
  pipeline type; sessions live in a `Vec` keyed by file key, ordered by
  open time (first = default). Opening a session = the do_pull-equivalent
  sequence at a concrete `open_store!` site inside the closure factory.
- **Watch:** one `Watcher` + backoff per session; the tick visits
  sessions round-robin (one meta poll per tick, deadline = interval /
  live-session-count, floor 2s) so total Tier-3 spend stays ≈ one file's
  worth per interval times the file count — well inside 50–150/min for
  dozens of files. `--no-watch` unchanged.
- **Cache / eviction:** unchanged — each session's store carries its own
  `proxy_cache`, evicted by that file's own version changes.
- **CLI:** unchanged single-file semantics (`--db`/`.figmog/current`);
  multi-file is a serve capability. `figmog call`/`tools` against a
  running multi-file config still address one store.
- **Proxied tools caveat (documented, not fixed):** the desktop server
  operates on the file open in the Figma app; the `file` argument does
  not route proxied tools. README states this plainly.

### Non-goals (v4)

Cross-file queries (joins/search spanning mirrors); mirroring whole
teams/projects by enumeration; eviction of idle sessions (a session
opened stays open for the process lifetime); CLI multi-file addressing.

### Testing

- Session-resolution unit tests (explicit file, default, unknown-file
  isError text, auto-open path with a scripted pull closure).
- Serve e2e: start with no FILE against two pre-built fixture stores'
  keys… (stores are per-key temp dirs; the e2e uses `--from-file`-built
  stores by pre-creating them under a temp `.figmog` root and passing
  `--figmog-root <dir>` — add that hidden flag for testability, default
  `.figmog`), then: `figmog_files` lists both, a tool with `file` routes
  to the right mirror (distinct fixture names prove it), omitted `file`
  errors when two mirrors exist and no default was given, `figmog_open`
  with `--from-file`-shaped… (network-free e2e: `figmog_open` is
  network-only; e2e covers its isError on missing token instead).

## 15. v5: the remote upstream (mcp.figma.com) — BLOCKED (2026-08-16)

> **Status: blocked by Figma policy, not engineering.** Verified via
> Figma community threads: (a) personal/plan access tokens are rejected
> at mcp.figma.com — OAuth is the only auth; (b) remote-MCP access is
> allowlisted to clients in Figma's MCP Catalog (VS Code, Cursor, Claude
> Code, Codex) — custom clients cannot request the `mcp:connect` scope
> and dynamic client registration returns 403. figmog therefore cannot
> authenticate as itself, and impersonating a catalog client's identity
> would circumvent Figma's access control — out of the question. The
> design below stands ready if Figma ever opens DCR/PAT auth; until
> then the desktop server is the only proxyable upstream, and remote-only
> capabilities are candidates for native REST-backed equivalents instead.

A second upstream flavor alongside the desktop server. The remote server
is a better proxy citizen than desktop — its tools take explicit
URLs/nodeIds per call (no selection), so proxied tools route per-file
like the local ones, erasing §12's open-file caveat — and it adds
remote-only tools (search_design_system, use_figma, whoami,
download_assets, generate_diagram, …). Its calls cost Tier-1-equivalent
per-minute budget on paid seats (6/month on Starter → effectively
paid-seat-only, consistent with the design center), which makes the
version-keyed cache genuinely valuable.

### Auth (the whole cost)

MCP OAuth, std-only:
- Discovery: on 401, read `WWW-Authenticate` /
  `/.well-known/oauth-protected-resource`, then the authorization
  server's metadata (`/.well-known/oauth-authorization-server`).
- Dynamic client registration at the advertised registration endpoint
  (public client, PKCE).
- Browser flow: local `TcpListener` on an ephemeral port serves the
  redirect; `open`/`xdg-open` launches the authorization URL; PKCE
  verifier from `/dev/urandom`, S256 challenge via a vendored ~100-line
  SHA-256 (well-known constants; unit-tested against published test
  vectors). State parameter checked.
- Tokens persisted at `<figmog-root>/auth.json` (0600), refresh-token
  flow on 401/expiry; failures degrade to "remote upstream
  unauthenticated" status (local tools unaffected).

### Surface

- `--upstream` accepts the remote URL; `--remote` sugar for
  `--upstream https://mcp.figma.com/mcp`. Desktop and remote are the
  same `UpstreamMcp` path — the OAuth layer is an `HttpUpstream`
  concern activated when a request meets a 401 challenge (desktop never
  does). `figmog login` CLI command runs the flow standalone;
  `figmog serve` triggers it lazily on first challenged request
  (browser opens once; stderr explains).
- Registry/routing/caching per §12 unchanged; remote tool descriptions
  prefixed "[via Figma remote] ". Cacheable rule unchanged (get_/list_
  + explicit node id) — remote's URL-addressed args satisfy it
  naturally; `use_figma`/creates are writes (uncached, meta-poll
  trigger).
- `figmog_status.upstream` distinguishes `connected (desktop)` /
  `connected (remote)` / `unauthenticated (remote)` / `unreachable` /
  `disabled`.

### Non-goals (v5)

Multiple simultaneous upstreams (one at a time via --upstream); token
encryption beyond file permissions; headless/device-code auth flows.

### Testing

SHA-256 against FIPS test vectors; PKCE challenge known-answer test;
OAuth state machine against a scripted in-process HTTP fake (challenge →
discovery → registration → token exchange → authenticated retry →
refresh-on-401); no live-network tests (manual live check documented).
