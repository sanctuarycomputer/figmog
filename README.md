# figmog

A fold-backed local mirror of one or more Figma files: `pull` fetches a
file once and keeps materialized indexes in a local database, so every
read after that — search, tree walks, component/style/variable queries —
answers from local storage in milliseconds, spending zero Figma API calls
and hitting zero rate limits. `figmog serve` runs the same engine as an
MCP stdio server with its own poll loop, plus a cached proxy to Figma's
native desktop MCP server, so it can be the only Figma MCP an agent needs
to connect.

See [`docs/SPEC.md`](docs/SPEC.md) for the full current-state spec
(architecture, data model, sync semantics, the MCP tool surface, the
cached proxy, variables, and the determinism/schema-stability contracts).

## License

figmog's own code is MIT (see [`LICENSE`](LICENSE)). It depends on
[`fold`](https://github.com/flowercomputers/bogkit) via a pinned git
dependency; upstream `bogkit` currently ships with no license file.
Building and running figmog locally from source is fine, but **broader
redistribution of binaries that embed `fold` waits on upstream adding a
license** — the release workflow lands ready, but publishing beyond this
repo's own testing releases is a deliberate, separate decision.

## Install

### From a release binary

Download the tarball for your platform from
[Releases](https://github.com/sanctuarycomputer/figmog/releases)
(`figmog-<version>-<target>.tar.gz` — `aarch64-apple-darwin`,
`x86_64-apple-darwin`, or `x86_64-unknown-linux-gnu`), then:

```console
$ tar xzf figmog-<version>-<target>.tar.gz
$ chmod +x figmog
$ ./figmog --help
```

**macOS: unsigned binary.** These builds are not code-signed or notarized
(no Apple Developer account in the loop yet), so Gatekeeper will refuse
to run the downloaded binary until you clear the quarantine attribute
once:

```console
$ xattr -d com.apple.quarantine ./figmog
```

### From source

Needs a Rust toolchain and network access (the build fetches `fold` from
its pinned git rev):

```console
$ git clone https://github.com/sanctuarycomputer/figmog.git
$ cd figmog
$ cargo build --release
$ ./target/release/figmog --help
```

Every `figmog ...` example below assumes a `figmog` binary on your
`PATH` (a release download, or `./target/release/figmog` after a
from-source build) — substitute `cargo run --release --` in place of
`figmog` if you'd rather run straight from a source checkout without
installing the binary anywhere, e.g. `cargo run --release -- pull <url>`.

## Quick start

```console
$ export FIGMA_TOKEN=figd_…            # figma.com → settings → security → personal access tokens
$ figmog pull "https://www.figma.com/design/<key>/<name>"
$ figmog search "pricing card"
$ figmog serve                          # MCP server with its own poll loop, in another terminal
```

After the first `pull`, figmog remembers the file key (in `.figmog/current`
under the current directory), so every later command — including all the
read commands below — can drop the file argument.

## Commands

Read commands never touch the network: they open the local store and read
one snapshot, and always print machine-readable JSON on stdout (pretty-
printed); errors always print `{"error": ...}` on stderr. `--db <path>`
(global) overrides the store location (default `.figmog/<file-key>/db`).

| command | reads | behavior |
|---|---|---|
| `figmog pull [file] [--from-file <json>] [--fresh]` | — | sync now; prints a churn summary (`+added ~changed -removed`). `file` is optional after the first pull. `--from-file` ingests a saved `GET /v1/files/:key` response instead of the network (offline ingestion, and what keeps the CLI tests hermetic). `--fresh` wipes the store **and the proxy response cache** and rebuilds from scratch. |
| `figmog serve [file...] [--interval N] [--no-watch] [--upstream <url>] [--no-upstream]` | — | MCP stdio server (see "Use from agents (MCP)" below); polls for changes itself (`--interval` seconds, default 10) and pulls only on an actual change; `--no-watch` disables that poll loop for a read-only, offline server; `--no-upstream` disables the cached proxy to Figma's native desktop MCP server |
| `figmog tools [--upstream <url>] [--no-upstream]` | — | list every tool `figmog serve` would expose for this mirror: name, source (`local`/`upstream`), and whether it's cache-capable. Never opens the store — works with no established mirror at all. |
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

Node ids accept both `12:34` and `12-34` forms everywhere. Auth is a
personal access token from `FIGMA_TOKEN`. Since `pull` and `serve`'s own
poll loop are the only things that touch the network, everything else
works fine with no token set as long as a store already exists.

## How sync works

`figmog serve`'s built-in poll loop polls the cheap `GET
/v1/files/:key/meta` endpoint (Tier 3) every `--interval` and only spends a
Tier-1 `GET /v1/files/:key` fetch — the expensive, rate-limited call — when
the file's content-modification watermark actually changes. Every fetch,
whether from `pull` or that poll loop, flows through fold's `KeyedStream`
upsert-diff: re-syncing a byte-identical node is a no-op through the whole
pipeline (zero graph churn, zero index writes), so a spurious trigger or a
repeated `pull` costs one Tier-1 fetch and nothing else. Since the
November 2025 rate-limit overhaul, file endpoints are capped around **10
requests/min on the free (Starter) plan**, and there is no delta API —
this polling design is what makes that budget workable for an agent that
wants to treat the file as live. The Tier-3 meta poll itself is capped
around **50 requests/min on Starter**, well above any sane `--interval`.

**Freshness is polling, not push.** figmog only learns a file changed the
next time it polls (or the next explicit `pull`) — there is no webhook or
subscription, so a change can take up to `--interval` seconds to be
reflected (Figma's own `FILE_UPDATE` webhook is Enterprise-only and
debounced up to 30 minutes even where it exists, so polling a cheap
Tier-3 endpoint is both simpler and faster in practice).

## Use from agents (MCP)

**figmog is the only Figma MCP an agent needs to connect.** `figmog serve`
is one process — fjall is single-writer, so a second writer would fight it
for the store lock — that owns the mirror, polls for changes itself, and
(unless `--no-upstream`) also attaches
Figma's native desktop MCP server as a **cached proxy**: `tools/list`
merges figmog's 19 local `figmog_*` tools with every tool the desktop
server advertises, verbatim, so an agent gets one server, one connection,
and the full native tool surface (`get_design_context`, `get_screenshot`,
`get_variable_defs`, code-generation tools, …) without figmog reimplementing
any of it. `figmog serve` also mirrors more than one file in one process —
see "Multiple files" below.

```console
$ claude mcp add figmog -- /absolute/path/to/figmog serve "https://www.figma.com/design/<key>/<name>"
```

Read-only / offline, once a store already exists (no `FIGMA_TOKEN`
needed):

```console
$ claude mcp add figmog -- /absolute/path/to/figmog serve --db .figmog/<key>/db --no-watch
```

`--interval N` (default 10s) controls the poll cadence of `serve`'s
built-in poll loop.

**Single-writer constraint:** because fjall allows only one open handle per
store, `figmog serve` holds an exclusive lock on its `--db` for as long as
it runs. A CLI read against the *same* store while `serve` is up —
`figmog status`, `figmog search`, `figmog call figmog_status`, and any
other command that opens the store — fails fast with a clean `store is
locked` error rather than a raw panic; drive the running server through
its own MCP tool calls instead, or stop `serve` first. (`figmog tools`
never opens the store, so it works fine even while `serve` is running.)
The same applies to `serve` itself: starting a second `figmog serve`
against a store one of them already owns fails with the same clean
message rather than a raw panic.

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
$ claude mcp add figmog -- /absolute/path/to/figmog serve
```

With more than one mirrored file, the poll loop round-robins: each tick
polls one session's Tier-3 meta endpoint, and the wait between ticks is
`--interval` split evenly across the mirrored files, floored at 2s — so
with many files the loop still tops out at 30 Tier-3 requests/min (well
under the ~50 req/min Starter cap above), it just polls each individual
file less often as the file count grows.

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

**Proxied tools caveat:** the desktop server operates on the file open in
the Figma app; the `file` argument does not route proxied tools. A `file`
argument sent on a non-`figmog_*` call is simply ignored — the desktop
server has no concept of "which file", so
`get_code`/`get_design_context`/etc. always answer for whatever file is
open in the Figma app, independent of any mirror `figmog serve` manages.

**Accepted divergence:** `.figmog/current` — the file `pull`/plain `figmog
serve <file>` remember so later CLI commands can drop the file argument —
is only refreshed by a startup pull that actually *ran*. A startup file
whose store is already populated (including every `--no-watch` invocation,
which never pulls at startup at all) leaves `.figmog/current` untouched;
only a genuine network pull — the initial pull against an empty store, or
a later poll-tick pull — writes it.

CLI commands (`pull`, `status`, and the rest) are unchanged and
still address exactly one file via `--db`/`.figmog/current` — multi-file
addressing is a `serve` capability only (no CLI multi-file addressing, no
cross-file queries, no idle-session eviction — a session opened stays
open for the process's life).

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
  `figmog_*` tools only.
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
  old version. A cache-write failure (practically unreachable in
  practice) is logged to stderr but never fails the call — the upstream
  response already succeeded and still reaches the client.
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

`figmog call` requires a resolved mirror — an established
`.figmog/current` (from a prior `pull`) or an explicit `--db <path>` —
since local tool dispatch reads the store; with neither, it exits 1 with
`no mirror here — run figmog pull <file-url> first`. `figmog tools` never
reads the store, so it works with no resolved mirror at all.

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

### `figmog_open` / `figmog_files`

Manage the multi-file mirror set directly (see "Multiple files" above):

| tool | input | answer |
|---|---|---|
| `figmog_open` | `file` (required) | mirror a file now (spends one Tier-1 pull); creates or re-syncs it; returns churn + node count |
| `figmog_files` | — | every mirrored file: key, name, version, node count, last synced time, and which one is the default |

That's 12 core read tools + 5 structural queries + 2 multi-file
management tools = **19 `figmog_*` tools** in total.

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
cache described above. `--no-upstream` recovers the "second, separate
server" shape if that's ever preferable — figmog's 19 `figmog_*` tools
alongside Figma's own, unrelated MCP connection.

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

## Manual live check

Not run in CI (needs a real `FIGMA_TOKEN` and a real file); this is how to
verify it by hand:

```console
$ export FIGMA_TOKEN=figd_…
$ figmog pull <figma url>
$ figmog status
$ figmog components
$ figmog search "pricing card"
$ figmog vars
```

`pull` pays the one Tier-1 fetch and prints a churn summary. Every command
after it — `status`, `components`, `search`, `vars` — is a local read:
acceptance is that they return in milliseconds, regardless of how large
the mirrored file is.

## Limitations

- **Variables are plan-gated** — on non-Enterprise plans (no automatic
  `variables/local` sync), inference (always on) covers each variable's
  default-mode value; a non-default mode's value is visible only where a
  frame explicitly overrides that mode. Full per-mode fidelity requires
  either the Enterprise auto-sync or a manual `import-variables`.
- **The desktop proxy needs the Figma app open on the right file** — the
  desktop MCP server has no concept of "which file"; every proxied
  (non-`figmog_*`) tool call answers for whatever file is currently open
  in the Figma desktop app, independent of any mirror `figmog serve`
  manages, and a `file` argument sent on such a call is silently ignored.
- **Freshness is polling, not push** — `figmog serve`'s poll loop polls
  the cheap `last_touched_at` metadata field on an interval; a change can
  take up to `--interval` seconds to be reflected, and Figma's
  `FILE_UPDATE` webhook (unavailable on the free plan, debounced up to 30
  minutes even where it exists) isn't used.
- **Single-writer store** — fjall allows only one open handle per store.
  `figmog serve` holds an exclusive lock on its `--db` for its whole
  life; a CLI command against the same store while `serve` is running
  fails fast with a clean `store is locked` error (exit 1), not a raw
  panic — drive the running server through its own MCP tool calls
  instead, or stop `serve` first.
- **No image renders** — figmog mirrors document structure and properties,
  not rendered pixels; there's no `GET /v1/images` integration.
- **Style definitions are derived, not authoritative** — the file JSON's
  `styles` map is metadata only (id, name, type), not the style's actual
  properties. `figmog styles --values` derives a definition from one
  consumer node's resolved properties (e.g. a text style's `TypeStyle`
  from a TEXT node that uses it) — if a style currently has no consumers,
  it has no derivable value.
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
