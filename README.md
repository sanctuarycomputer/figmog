# figmog

A fold-backed local mirror of one Figma file: `pull` fetches it once and
keeps materialized indexes in a fold database, so every read after that —
search, tree walks, component/style/variable queries — answers from local
storage in milliseconds, spending zero Figma API calls and hitting zero
rate limits.

## Quick start

```console
$ export FIGMA_TOKEN=figd_…            # figma.com → settings → security → personal access tokens
$ cargo run -p figmog -- pull "https://www.figma.com/design/<key>/<name>"
$ cargo run -p figmog -- search "pricing card"
$ cargo run -p figmog -- watch          # keep it fresh in another terminal
```

After the first `pull`, figmog remembers the file key (in `.figmog/current`
under the current directory), so every later command — including `watch`
and all the read commands below — can drop the file argument.

## Commands

Read commands never touch the network: they open the local store and read
one snapshot. `--json` (global) emits machine-readable JSON on stdout
instead of the human-readable format; `--db <path>` (global) overrides the
store location (default `.figmog/<file-key>/db`).

| command | reads | behavior |
|---|---|---|
| `figmog pull [file] [--from-file <json>] [--fresh]` | — | sync now; prints a churn summary (`+added ~changed -removed`). `file` is optional after the first pull. `--from-file` ingests a saved `GET /v1/files/:key` response instead of the network (offline ingestion, and what keeps the CLI tests hermetic). `--fresh` wipes the store **and the proxy response cache** and rebuilds from scratch. |
| `figmog watch [file] [--interval N]` | — | poll loop: cheap metadata check every `N` seconds (default 10), full pull only on an actual change |
| `figmog serve [file] [--interval N] [--no-watch] [--upstream <url>] [--no-upstream]` | — | MCP stdio server (see "Use from agents (MCP)" below); `--no-watch` disables the poll loop for a read-only, offline server; `--no-upstream` disables the cached proxy to Figma's native desktop MCP server |
| `figmog tools [--upstream <url>] [--no-upstream]` | — | list every tool `figmog serve` would expose for this mirror: name, source (`local`/`upstream`), and whether it's cache-capable |
| `figmog call <tool> [--args '<json>'] [--upstream <url>] [--no-upstream]` | — | invoke any tool by name through the same dispatch `figmog serve` uses — local `figmog_*` tools and, when attached, any upstream tool |
| `figmog status` | meta + nodes | file name, version, last modified, node count |
| `figmog pages` | by_type + nodes | list CANVAS pages (id, name) |
| `figmog tree [id] [--depth N]` | children + nodes (+ by_type to find the root) | indented outline: `name  [type]  id`; root defaults to the DOCUMENT node |
| `figmog get <id> [--children]` | nodes (+ children) | the full `raw` JSON of a node; `--children` inlines one level of child summaries |
| `figmog find --type <TYPE> [--page <id>]` | by_type + nodes | nodes by type, optional page filter |
| `figmog search <query> [-n N]` | Bm25 + nodes | ranked hits (default 10): score, id, type, name, page, text snippet |
| `figmog instances <id\|key\|name>` | nodes + instances_of + components + component_sets | resolve the argument to a component (by node id, global key, or a unique component/component-set name — a set name expands to all its variants), list instance nodes |
| `figmog components` | components + component_sets + nodes | design-system inventory: component sets with their variant axes, standalone components |
| `figmog styles [--type <t>] [--values]` | styles + styled_by (+ nodes) | styles with usage counts; `--values` derives each style's definition from a consumer node (§ below) |
| `figmog uses <id>` | styled_by / bound_to + nodes | nodes using a style id or bound to a variable id |
| `figmog vars [id]` | nodes + variables + variable_collections | variables: authoritative record if imported, else inferred value(s) + binding sites |
| `figmog import-variables <path>` | — | upsert variable/collection records from a variables export (see "Variables") |
| `figmog stats` | nodes + by_type + components + component_sets + styles + variables | node counts by type and by page, component/set/style/variable totals, text-node count, max tree depth — whole-file structural queries the API can't offer at all |
| `figmog path <id>` | nodes | ancestor chain root→node: `[{id, name, type}]` |
| `figmog text [--page <id>]` | by_type + nodes | every TEXT node's `(id, characters, page_id)`, sorted by id |
| `figmog where --pointer </p> [--equals <json>] [--page <id>]` | nodes | nodes whose raw JSON matches an RFC 6901 `pointer`, optionally filtered by `equals` (parsed as JSON, falling back to a bare string so `--equals VERTICAL` works) |
| `figmog at --x N --y N` | nodes | nodes whose absolute bounds contain the point, sorted by area ascending (deepest/smallest first) |
| `figmog bench [file] [--nodes N] [--calls M] [--api-calls K] [--skip-api] [--keep] [--interactive]` | — | self-contained load-test demo, or (`--interactive`) a live REPL — see "Demo: load-testing the server" below — needs no mirror/`--db` |

Node ids accept both `12:34` and `12-34` forms everywhere. Auth is a
personal access token from `FIGMA_TOKEN`. Since `pull`/`watch` are the only
commands that touch the network, everything else works fine with no token
set as long as a store already exists.

## How sync works

`watch` polls the cheap `GET /v1/files/:key/meta` endpoint (Tier 3) every
interval and only spends a Tier-1 `GET /v1/files/:key` fetch — the
expensive, rate-limited call — when the file's content-modification
watermark actually changes. Every fetch, whether from `pull` or `watch`,
flows through fold's `KeyedStream` upsert-diff: re-syncing a byte-identical
node is a no-op through the whole pipeline (zero graph churn, zero index
writes), so a spurious trigger or a repeated `pull` costs one Tier-1 fetch
and nothing else. Since the November 2025 rate-limit overhaul, file
endpoints are capped around **10 requests/min on the free (Starter)
plan**, and there is no delta API — this polling design is what makes
that budget workable for an agent that wants to treat the file as live.
The Tier-3 meta poll itself is capped around **50 requests/min on
Starter**, well above any sane `--interval`.

## Use from agents (MCP)

**figmog is the only Figma MCP an agent needs to connect.** `figmog serve`
is one process — fjall is single-writer, so a standalone MCP server would
fight `figmog watch` for the store lock — that owns the mirror, polls for
changes exactly like `watch`, and (unless `--no-upstream`) also attaches
Figma's native desktop MCP server as a **cached proxy**: `tools/list`
merges figmog's 19 local `figmog_*` tools with every tool the desktop
server advertises, verbatim, so an agent gets one server, one connection,
and the full native tool surface (`get_design_context`, `get_screenshot`,
`get_variable_defs`, code-generation tools, …) without figmog reimplementing
any of it. `figmog serve` also mirrors more than one file in one process —
see "Multiple files" below.

```console
$ cargo build -p figmog
$ claude mcp add figmog -- /absolute/path/to/clog/target/debug/figmog serve "https://www.figma.com/design/<key>/<name>"
```

Read-only / offline, once a store already exists (no `FIGMA_TOKEN`
needed):

```console
$ claude mcp add figmog -- /absolute/path/to/clog/target/debug/figmog serve --db .figmog/<key>/db --no-watch
```

`--interval N` (default 10s) controls the poll cadence, same as `watch`.

**Single-writer constraint:** because fjall allows only one open handle per
store, `figmog serve` (like `figmog watch`) holds an exclusive lock on its
`--db` for as long as it runs. A CLI read against the *same* store while
`serve` is up — `figmog status`, `figmog search`, `figmog call
figmog_status`, and any other command that opens the store — fails fast
with a clean `store is locked` error rather than a raw panic; drive the
running server through its own MCP tool calls instead, or stop `serve`
first. (`figmog tools` never opens the store, so it works fine even while
`serve` is running.) The same applies to `serve`/`watch` itself: starting
a second `figmog serve` or `figmog watch` against a store one of them
already owns fails with the same clean message rather than a raw panic.

### Multiple files

`figmog serve [FILE]...` takes zero or more Figma file URLs/keys at
startup — one process can mirror several files. Every local `figmog_*`
tool gains an optional `file` argument (a URL or bare key, parsed the same
way as the CLI's own file arguments): pass it to route the call at a
specific mirror, auto-opening it (spending one Tier-1 pull) the first time
an agent references it; omit it and the call answers from the *default*
file — the first `FILE` given at startup, or whichever got mirrored first
if none were.

```console
$ claude mcp add figmog -- /absolute/path/to/clog/target/debug/figmog serve
```

Zero files at startup is valid and needs no `FIGMA_TOKEN` up front — the
server starts empty and mirrors files as an agent references them by URL.
This is the shape to reach for with `claude mcp add` when you don't want
to commit to one file ahead of time; passing one or more files at startup
(as in the single-file examples above) still works exactly as before, and
the first one becomes the default so every existing single-file tool call
still needs no `file` argument at all.

Two tools manage the mirror set directly:

- `figmog_open {file}` — mirror a file now (spends one Tier-1 pull);
  returns its churn and node count. Creates the mirror if it's new, or
  re-syncs it if already mirrored.
- `figmog_files` — list every mirrored file: key, name, version, node
  count, last synced time, and which one (if any) is the default.

**Proxied tools caveat (spec §14, verbatim):** "the desktop server
operates on the file open in the Figma app; the `file` argument does not
route proxied tools." A `file` argument sent on a non-`figmog_*` call is
simply ignored — the desktop server has no concept of "which file", so
`get_code`/`get_design_context`/etc. always answer for whatever file is
open in the Figma app, independent of any mirror `figmog serve` manages.

**Accepted divergence:** `.figmog/current` — the file `pull`/`watch`/plain
`figmog serve <file>` remember so later CLI commands can drop the file
argument — is only refreshed by a startup pull that actually *ran*. A
startup file whose store is already populated (including every
`--no-watch` invocation, which never pulls at startup at all) leaves
`.figmog/current` untouched; only a genuine network pull — the initial
watch-mode pull against an empty store, or a later watch-tick pull —
writes it.

CLI commands (`pull`, `watch`, `status`, and the rest) are unchanged and
still address exactly one file via `--db`/`.figmog/current` — multi-file
addressing is a `serve` capability only (spec §14 non-goal: no CLI
multi-file addressing, no cross-file queries, no idle-session eviction —
a session opened stays open for the process's life).

### The cached proxy

Proxying targets **paid Dev/Full seats**: it requires the Figma desktop
app to be running with its Dev Mode MCP server enabled (streamable HTTP,
default `http://127.0.0.1:3845/mcp`). At startup figmog probes it; on
success, every non-`figmog_*` tool call is forwarded there. On failure
(desktop app not running, no Dev/Full seat, wrong URL), figmog logs one
stderr line and falls back to local-only tools for the rest of the
process — no mid-session re-probe, so restart `figmog serve` once the
desktop server is reachable to attach it.

- `--upstream <url>` overrides the desktop server's URL.
- `--no-upstream` disables proxying entirely — figmog serves its 19
  `figmog_*` tools only, exactly like v2 (plus v4's multi-file surface).
- **Namespace rule:** `figmog_*` tools are always local; every other tool
  name is always proxied. If the desktop server ever advertised a tool
  named `figmog_*`, figmog would drop it and log a warning rather than
  let it collide — this can't happen with figmog's own registry, but a
  live desktop server's tool list is outside figmog's control.
- **Cacheable rule:** a proxied call is served from (and written to) a
  version-keyed response cache when its tool name starts `get_`/`list_`
  **and** its arguments carry an explicit node id (`nodeId`, `node_id`,
  or `id`, as a string) — e.g. `get_code` with a `nodeId` hits the cache
  on a repeat call for the same node, as long as the mirror's file
  version hasn't changed since. Selection-based calls (no explicit node
  id) are always forwarded live. A version bump (from a pull, whether the
  poll loop's or `figmog_sync`'s) evicts every cache row tagged with the
  old version.
- **Only two things spend Figma's API/rate budget:** `figmog_sync` (a
  forced pull) and any proxied, native-named tool call that reaches the
  desktop server (a cache hit doesn't). Every `figmog_*` read tool is
  free, whether or not the proxy is attached.
- A successful proxied call to a tool that isn't `get_*`/`list_*` (e.g. a
  code-connect write) may have changed the file, so figmog schedules an
  immediate meta-poll rather than waiting for the next `--interval` tick
  (skipped in `--no-watch` mode, which has no poll loop to schedule).
- `pull --fresh` wipes the store **and** the proxy response cache — a
  totally clean rebuild.

### CLI parity

Every tool figmog serves — local or proxied — is also reachable from the
CLI, so you can inspect or drive the exact same dispatch without an MCP
client:

```console
$ figmog tools                              # merged list: name, source, cacheable
$ figmog call figmog_search --args '{"query": "pricing card"}'
$ figmog call get_code --args '{"nodeId": "1:2"}'   # proxied, cached by version
```

Both accept `--upstream <url>` / `--no-upstream`, probed fresh per
invocation (no persistent connection between CLI calls). There are
deliberately no bespoke subcommands for upstream tools — Figma's tool
list churns; `figmog call` is the stable, generic surface.

`figmog tools` and `figmog call` both require a resolved mirror — an
established `.figmog/current` (from a prior `pull`) or an explicit `--db
<path>` — even though `figmog tools` itself never reads the store; with
neither, both exit 1 with `no mirror here — run figmog pull <file-url>
first`.

### Core read tools

Each mirrors a CLI read command one-to-one and answers instantly from the
local store — zero Figma API cost, zero rate-limit exposure. Every tool
below also takes an optional `file` argument (URL or key) routing the call
at a specific mirror — see "Multiple files" above.

| tool | input | reads |
|---|---|---|
| `figmog_status` | — | file name, version, last modified, node count |
| `figmog_pages` | — | list CANVAS pages (id, name), in document order |
| `figmog_tree` | `id`, `depth` | subtree outline rooted at a node; root defaults to the document |
| `figmog_node` | `id` (required), `children` | full `raw` JSON of one node; `children` inlines a one-level summary |
| `figmog_find` | `type` (required), `page` | nodes by Figma node type, optionally scoped to one page |
| `figmog_search` | `query` (required), `limit` | BM25 search over layer names and text content |
| `figmog_instances` | `target` (required) | instances of a component, resolved by node id, key, or (set) name |
| `figmog_components` | — | design-system inventory: sets with variant axes, standalone components |
| `figmog_styles` | `type`, `values` | styles with usage counts; `values` derives each definition from a consumer |
| `figmog_uses` | `id` (required) | nodes using a style id or bound to a variable id |
| `figmog_vars` | `id` | variables: authoritative if imported, else inferred from bindings |
| `figmog_sync` | — | forces one pull and returns the churn — the **only** tool that spends Figma's rate budget |

### Whole-file structural queries

The local mirror's unfair advantage: full-file answers no rate-limited API
surface could offer, each a read-only scan/join over the same indexes.
Every one has a matching CLI subcommand, so the CLI/tool surface stays
one-to-one, and (like the core read tools above) each also takes an
optional `file` argument.

| tool | CLI equivalent | input | answer |
|---|---|---|---|
| `figmog_stats` | `figmog stats` | — | node counts by type/page, component/set/style/variable totals, text-node count, max tree depth |
| `figmog_path` | `figmog path <id>` | `id` (required) | ancestor chain root→node as `[{id, name, type}]` |
| `figmog_text` | `figmog text [--page id]` | `page` | every TEXT node's `(id, characters, page_id)`, sorted by id |
| `figmog_where` | `figmog where --pointer /p --equals <json>` | `pointer` (required, RFC 6901 into `raw`), `equals`, `page` | matching `[{id, name, type, page_id, value}]`, sorted by id |
| `figmog_at` | `figmog at --x N --y N` | `x`, `y` (required) | nodes whose `abs_bounds` contain the point, sorted by area ascending (deepest/smallest first) |

### Relationship to Figma's official MCP server

figmog **replaces** the official desktop MCP server in an agent's config —
connect figmog instead of it, not alongside it. figmog's `initialize`
response carries steering `instructions` telling an agent to reach for
figmog for everything:

> figmog is your Figma server: a local, instant mirror of one Figma file
> plus a cached proxy to Figma's native capabilities. Call figmog for
> everything Figma-related. figmog_* tools answer from the local mirror
> at zero API cost; native-named tools (get_*, …) go to Figma, cached by
> file version where possible. Pass the Figma file URL as the `file`
> argument when you have one; figmog mirrors files on first reference.

Every figmog-native tool lives in the `figmog_*` namespace, so it never
collides by name with a proxied tool; local tools only ever read the
mirror and only ever write to it via `figmog_sync`, the one local tool
that spends Figma's Tier-1 rate budget (a forced pull) — every other
local tool call is instant, free, and backed by the same
fold-materialized indexes the CLI reads. Proxied tools go through the
cache described above. `--no-upstream` recovers the older, "second,
separate server" shape (v2) if that's ever preferable — figmog's 19
`figmog_*` tools alongside Figma's own, unrelated MCP connection.

## Variables

**Enterprise auto-sync (automatic, zero setup).** Every network `pull`
additionally calls `GET /v1/files/:key/variables/local` — the Enterprise
REST endpoint for full-fidelity variable and collection records:
collections, modes (e.g. light/dark), per-mode values, descriptions,
scopes. When it succeeds, those records are folded into the same sync and
kept live: a variable removed upstream is swept on the next pull, exactly
like a deleted node. On non-Enterprise plans the endpoint 403s (or 404s)
and the call is **silently skipped** — no error, no flag to set — falling
back to the two paths below. `--from-file` pulls never call it at all
(no network involved).

Below that, variables are supported through two complementary fallback
paths that work on every plan:

**Path 1 — mirrored bindings + inference (always on, zero setup).** Every
variable-bound property in the file JSON carries a `boundVariables`
reference, and Figma bakes the resolved concrete value into the same node
next to it. figmog scans every node for these bindings at every depth and
inverts them into a `bound_to` index. `figmog vars` aggregates at read
time: for each variable id, every binding site (node + property path) and
the observed value(s) baked in there. This covers each variable's
**default-mode value**; values from a non-default mode appear only where a
frame explicitly overrides its mode.

**Path 2 — manual import (optional).** `figmog import-variables
<export.json>` upserts the same full-fidelity variable and collection
records the Enterprise auto-sync produces, by hand. It accepts two shapes:
the Enterprise REST `variables/local` response (the same shape auto-sync
already ingests, useful for a one-off import outside `pull`), or the JSON
produced by the free-plan escape hatch below — the Figma Plugin API can
read local variables on **any** plan, run from Figma's own developer
console. `figmog vars` prefers an authoritative record (auto-synced or
imported) over inference whenever one exists. Unlike auto-synced records,
manually imported ones are **not** swept by a later pull that has no
Enterprise export of its own (e.g. on a non-Enterprise plan) — they
persist until re-imported or `pull --fresh`.

```js
// Figma → Plugins → Development → Open console, then paste:
(async () => {
  const collections = await figma.variables.getLocalVariableCollectionsAsync();
  const variables = await figma.variables.getLocalVariablesAsync();
  const out = { variables: {}, variableCollections: {} };
  for (const c of collections)
    out.variableCollections[c.id] = { id: c.id, name: c.name, modes: c.modes, defaultModeId: c.defaultModeId };
  for (const v of variables)
    out.variables[v.id] = { id: v.id, name: v.name, resolvedType: v.resolvedType,
                            variableCollectionId: v.variableCollectionId,
                            valuesByMode: v.valuesByMode, description: v.description, scopes: v.scopes };
  console.log(JSON.stringify(out));
})();
// save the logged JSON, then: figmog import-variables vars.json
```

A third source, for paid Dev/Full seats: `figmog serve`'s cached proxy
(see "Use from agents (MCP)" above) forwards `get_variable_defs` to
Figma's desktop server like any other native-named tool, selection-scoped
and cached by file version like the rest of the proxy. It's still
selection-scoped rather than whole-collection, so `import-variables`
remains the way to get an authoritative, whole-collection record into the
mirror; anyone with a paid seat can pipe a proxied `get_variable_defs`
call's output into it by hand. Figma's *remote* MCP server (as opposed to
the local desktop one figmog proxies) caps Starter users at 6 tool calls
a *month* and isn't something figmog talks to at all.

## Demo: load-testing the server

`figmog bench` makes the value proposition measurable without needing a
real Figma file or token — one self-contained command:

```console
$ cargo run --release -p figmog -- bench
```

It runs four phases against a fresh temp store (cleaned up afterward
unless `--keep`), all `Instant`-timed:

1. **Corpus** — a deterministic synthetic Figma file (`--nodes`, default
   10000): pages of auto-layout frames, TEXT nodes drawn from a fixed
   64-word pool (so BM25 has real queries), a "Button" `COMPONENT_SET`
   with variants and INSTANCE nodes referencing them, fill/text styles,
   and `boundVariables` bindings. A seeded LCG makes it byte-identical
   across runs of the same `--nodes` — no wall-clock, no `rand` dependency.
2. **Cold sync** — `flatten` + `sync` the corpus into the temp store via
   the library (not a subprocess).
3. **No-churn re-pull** — `sync` the identical corpus again and assert in
   code that churn is zero: the engine's headline invariant, timed.
4. **Serve load** — spawn the real `figmog serve --no-upstream --no-watch`
   binary and drive it over its actual stdio pipe with `--calls` (default
   5000) tool calls in a fixed rotating mix (`figmog_search`,
   `figmog_node`, `figmog_where`, `figmog_stats`, `figmog_tree`,
   `figmog_instances`) — every parameter (search words, node ids, the
   instances target) is *derived* from the corpus's own flattened
   records, not hardcoded. Sequential over one pipe, matching the
   server's real single-threaded loop, so the numbers are honest.

Sample output (this machine: Apple M4, 16GB, dev profile — `cargo run -p
figmog -- bench --nodes 10000 --calls 5000`; dev profile is `opt-level =
3` in this workspace, so the numbers are respectable even without
`--release`):

```
corpus  [synthetic]  10000 nodes, 2391449 bytes, 44.7ms
cold sync    37.3ms flatten + 98.9ms sync, 10005 records (101140 records/s)
re-pull      7.0ms, churn zero: true

tool                  calls   p50 (ms)   p95 (ms)   p99 (ms)   max (ms)
figmog_search           834      0.399      0.531      0.911      3.654
figmog_node             834      0.043      0.058      0.152      8.374
figmog_where            833     15.302     17.161     25.223     48.124
figmog_stats            833     24.089     27.427     47.298    160.400
figmog_tree             833      3.971      5.014      9.264     20.122
figmog_instances        833      0.749      0.903      1.647      4.295

figmog served 5000 queries in 38.4s (130 req/s). Figma's Tier-1 API budget on a free plan: ~10 file requests per MINUTE.
```

(`figmog_node` — an indexed point lookup — is the fastest tool by a wide
margin; `figmog_where`/`figmog_stats` scan every node and are
correspondingly slower, but still complete a 5000-call load test in
under 40 seconds against a 10000-node file. All of it is local: zero
Figma API calls, zero rate-limit exposure.)

**Against a real file** (`figmog bench <file-url-or-key>`, needs
`FIGMA_TOKEN`): the corpus becomes the real file — fetched once (exactly
one Tier-1 call, plus the same opportunistic Enterprise `variables_local`
call `pull` makes), reused in memory for the re-pull phase (no second
fetch) — and the load test's query mix derives its parameters from
whatever's actually in that file (falling back gracefully, e.g. dropping
`figmog_instances` from the mix with a stderr note if the file has no
components). Unless `--skip-api`, a fifth phase follows the serve load:
`--api-calls` (default 5) sequential `GET /v1/files/:key/nodes?ids=`
calls — Figma's native equivalent of `figmog_node` — timed the same way,
plus one `GET /meta` call for reference, so the report can show
`figmog_node` p50 next to the real API's p50 side by side, with a
speedup factor and the budget math (how long the same `--calls` load
test would take at Figma's ~10 Tier-1 requests/minute). **This spends
real rate-limit budget**: 1 file fetch + 1 opportunistic
`variables_local` + `K` `/nodes` calls + 1 `/meta` call — the report
states every call it made; a 429 mid-phase is recorded (with its
`Retry-After`) and ends the phase gracefully rather than failing the
whole bench.

### Interactive mode (`--interactive`)

`figmog bench [file] --interactive` runs the same setup (corpus/real file
→ cold sync → no-churn re-pull → serve child spawn) and then, instead of
the automated load/API phases, drops into a REPL so requests are visible
as they fire — one aligned line per call: sequence number, tool, arg
digest, latency, and a result digest. Colors (green under 10ms, yellow
under 100ms, red above; errors always red) are raw ANSI, emitted only
when stdout is a real terminal — piped/non-TTY output (CI, this README's
transcripts) is always plain text.

| command | does |
| --- | --- |
| `search <words…>` | BM25 search over layer names/text |
| `node <id> [children]` | full node JSON |
| `tree [id] [depth]` | subtree outline |
| `find <TYPE> [page]` | nodes by Figma node type |
| `where <pointer> [value]` | nodes matching an RFC 6901 pointer (`value` parsed as JSON, falling back to a bare string) |
| `stats` | node counts, totals, max depth |
| `path <id>` | ancestor chain to a node |
| `text [page]` | every TEXT node's characters |
| `at <x> <y>` | nodes containing a point |
| `instances <target>` | instances of a component |
| `components` | design-system inventory |
| `styles [type]` | styles with usage counts |
| `uses <id>` | nodes using a style/variable id |
| `vars [id]` | variables |
| `pages` | list pages |
| `status` | file name/version/node count |
| `run <N>` | fire N requests of the derived mixed workload, streaming each line live, then a burst percentile table |
| `report` | cumulative per-tool percentiles for everything fired this session |
| `api node <id>` / `api meta` | real-file mode only — one live Figma API call (`/nodes` or `/meta`), timed the same way and labeled with the API cost it spent; a 429 prints its `Retry-After` in red and the REPL keeps going |
| `call <tool> <json-args>` | raw escape hatch (works for proxied tools too, when an upstream is attached) |
| `help` | this table |
| `quit` | exit cleanly (EOF also works — the serve child is always reaped, never left a zombie) |

Sample transcript (same machine as above: Apple M4, 16GB, `cargo run
--release -p figmog -- bench --nodes 10000 --interactive`, piped
non-interactively so this is plain text — a real terminal shows it in
color):

```console
$ printf 'search garden\nnode 1:1\nrun 8\nreport\nquit\n' \
  | cargo run --release -p figmog -- bench --nodes 10000 --interactive
corpus  [synthetic]  10000 nodes, 2391449 bytes, 54.7ms
cold sync    36.9ms flatten + 139.2ms sync, 10005 records (71850 records/s)
re-pull      6.3ms, churn zero: true

figmog bench --interactive — type `help` for commands, `quit` to exit.
#   1  figmog_search      {"query":"garden"}                   0.07ms  0 hits
#   2  figmog_node        {"id":"1:1"}                         0.08ms  Button
#   3  figmog_search      {"query":"Nav"}                      0.42ms  10 hits
#   4  figmog_node        {"id":"25:177"}                      0.04ms  Slider Body Banner Table Toolbar Button
#   5  figmog_where       {"equals":"VERTICAL","pointer":"    13.51ms  1809 hits
#   6  figmog_stats       {}                                  20.49ms  ok
#   7  figmog_tree        {"depth":2}                          3.37ms  Document
#   8  figmog_instances   {"target":"Button"}                  0.66ms  407 hits
#   9  figmog_search      {"query":"12"}                       0.04ms  1 hits
#  10  figmog_node        {"id":"37:78"}                       0.05ms  Grid Field Preview Progress Divider Toggle Row Header

tool                  calls   p50 (ms)   p95 (ms)   p99 (ms)   max (ms)
figmog_search             2      0.040      0.040      0.040      0.422
figmog_node               2      0.041      0.041      0.041      0.051
figmog_where              1     13.508     13.508     13.508     13.508
figmog_stats              1     20.490     20.490     20.490     20.490
figmog_tree               1      3.369      3.369      3.369      3.369
figmog_instances          1      0.656      0.656      0.656      0.656

tool                  calls   p50 (ms)   p95 (ms)   p99 (ms)   max (ms)
figmog_search             3      0.071      0.071      0.071      0.422
figmog_node               3      0.051      0.051      0.051      0.076
figmog_where              1     13.508     13.508     13.508     13.508
figmog_stats              1     20.490     20.490     20.490     20.490
figmog_tree               1      3.369      3.369      3.369      3.369
figmog_instances          1      0.656      0.656      0.656      0.656
```

`search garden`/`node 1:1` are the first two typed commands; `run 8`
streams 8 requests of the derived mixed workload live (lines 3-10) then
prints its own burst table; `report` prints the session's cumulative
table (same six tools, now 3 `figmog_search`/3 `figmog_node` calls
counted). `quit` closes stdin to the serve child and waits for it to
exit — no zombie process left behind.

**Against a real file** (`figmog bench <file> --interactive`, needs
`FIGMA_TOKEN`), the `api node <id>` / `api meta` commands become the
demo's centerpiece: firing one alongside a `node <id>` for the same id
puts figmog's local read and Figma's real Tier-1 API call side by side,
live. Illustrative shape (not a captured run — no token in this repo's
CI/dev environment — but the format is exactly what `format_latency_line`
and the `api node` line print):

```
#  11  figmog_node        {"id":"1:234"}                       0.05ms  Icon/Star
#  12  API node           1:234                              ~400.00ms  ok  (spent 1 Tier-1 call)
```

figmog's read is a local, indexed point lookup (sub-millisecond); the API
call pays a real network round trip — that gap, live, is the whole
pitch. Every `api …` call spends real rate-limit budget (Figma's Tier-1 files
allow ~10/minute) — the line's `(spent …)` note says exactly what it
cost, and a 429 prints its `Retry-After` in red instead of exiting.

## Manual live check

Not run in CI (needs a real `FIGMA_TOKEN` and a real file); this is how to
verify it by hand:

```console
$ export FIGMA_TOKEN=figd_…
$ cargo run -p figmog -- pull <figma url>
$ cargo run -p figmog -- status
$ cargo run -p figmog -- components
$ cargo run -p figmog -- search "pricing card"
$ cargo run -p figmog -- vars
```

`pull` pays the one Tier-1 fetch and prints a churn summary. Every command
after it — `status`, `components`, `search`, `vars` — is a local read:
acceptance is that they return in milliseconds, regardless of how large
the mirrored file is.

## Limitations

- **Variables** — on non-Enterprise plans (no automatic `variables/local`
  sync), inference (always on) covers each variable's default-mode value;
  a non-default mode's value is visible only where a frame explicitly
  overrides that mode. Full per-mode fidelity requires either the
  Enterprise auto-sync or a manual `import-variables`.
- **No image renders** — figmog mirrors document structure and properties,
  not rendered pixels; there's no `GET /v1/images` integration.
- **Style definitions are derived, not authoritative** — the file JSON's
  `styles` map is metadata only (id, name, type), not the style's actual
  properties. `figmog styles --values` derives a definition from one
  consumer node's resolved properties (e.g. a text style's `TypeStyle`
  from a TEXT node that uses it) — if a style currently has no consumers,
  it has no derivable value.
- **Change detection is polling, not webhooks** — `watch` polls the cheap
  `last_touched_at` metadata field on an interval; Figma's `FILE_UPDATE`
  webhook is unavailable on the free plan and debounced up to 30 minutes
  even where it exists, so polling a cheap Tier-3 endpoint is both
  simpler and faster.
- **Instance overrides beyond the serialized subtree are not resolved** —
  Figma serializes an INSTANCE's overridden children as ordinary nodes
  under it, and those mirror like any other node, but overrides that
  Figma doesn't materialize into the subtree are not reconstructed.
- **`pull --fresh` wipes imported variables** — `--fresh` deletes the whole
  store, including `import-variables` records that normally survive
  ordinary pulls (they're exempt from the file-sync sweep, not from a full
  wipe). On an Enterprise plan the very next `pull` repopulates them
  automatically (auto-sync); everywhere else, re-run `import-variables`
  after a `--fresh` pull if you need authoritative variable data back.
