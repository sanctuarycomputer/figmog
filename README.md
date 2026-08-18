# figmog

figmog is a superset of the Figma MCP surface: a local, instantly-queryable
mirror of your Figma files built for high-performance design agents.

One node read: 0.055ms from the mirror, 972.6ms through Figma's REST API in
the same run. That is 17,657x faster. figmog sustained 149 requests a second
where Figma's Tier-1 file budget is about 10 requests a minute, and
re-syncing a file nobody had edited cost 4.6ms and wrote nothing. Measured
2026-08-17 against a real 5,339-node production file; the full run is
[further down](#the-benchmark-run).

## Quick start

### 1. Get the binary

```console
$ tar xzf figmog-<version>-<target>.tar.gz
$ chmod +x figmog
$ xattr -d com.apple.quarantine ./figmog   # macOS only, see Install
```

Tarballs live on the
[releases page](https://github.com/sanctuarycomputer/figmog/releases).

### 2. Set a Figma token

```console
$ export FIGMA_TOKEN=figd_…
```

Make one at figma.com under settings, security, personal access tokens.

### 3. Connect it to Claude Code

```console
$ claude mcp add figmog -- $PWD/figmog serve
```

### 3b. Remove the official Figma MCP server, if you have one connected

```console
$ claude mcp list        # find its name; it is usually `figma`
$ claude mcp remove figma
```

Run one or the other, not both. figmog already forwards the official
server's tools, with caching, whenever the Figma desktop app is running, so
dropping the direct connection loses nothing. Keep both and the agent sees
two overlapping Figma servers, then picks tools badly.

### 4. Ask for something

> Using figmog, pull this file: [figma file URL], then build this frame in
> HTML & CSS: [figma frame URL]

figmog mirrors the file the first time the agent references it, and every
read after that answers from local storage. Frame URLs carry `?node-id=…`,
which figmog's tools accept directly, so paste the URL straight out of
Figma.

## The CLI, for humans

```console
$ figmog pull "https://www.figma.com/design/<key>/<name>"
$ figmog search "pricing card"
$ figmog dump 1:23 --fields id,name,type,fills --depth 3
$ figmog serve                     # the MCP server, in another terminal
```

The first `pull` writes the file key to `.figmog/current`, so later commands
can drop the file argument. Read commands never touch the network and print
JSON on stdout; failures print `{"error": ...}` on stderr and exit 1. Every
command is documented in [`docs/SPEC.md`](docs/SPEC.md) §10.

## figmog against the API

| | figmog | Figma REST API and MCP |
|---|---|---|
| node read, p50 | 0.055ms | 972.6ms |
| sustained rate | 149 req/s | ~10 file requests/min on the free plan |
| second read of the same node | free | spends the budget again |
| counts, pointer matches, every text node | one local scan | no endpoint offers them |
| re-sync of an unedited file | 4.6ms, zero churn | one Tier-1 fetch |

Three things spend Figma's budget: a pull (`figmog pull`, the poll loop's own
sync, `figmog_sync`, `figmog_open`), an explicit `figmog images` fetch, and a
proxied call that reaches the desktop app. Every `figmog_*` read tool is
free.

## What the mirror gives an agent

### Reads work while the server runs

`figmog serve` owns the store, and it listens on a unix socket at
`.figmog/serve.sock`. Read commands, `figmog tools` and `figmog call` connect
there first and answer from the running server's live view; with no server
listening they open the store directly. `--no-socket` forces direct mode on
both sides. `pull` is a writer and always opens the store itself, so while a
server runs its error points at `figmog call figmog_sync`, which reaches the
process that owns the store. See `docs/SPEC.md` §6 and §10.

### One process, many files

`figmog serve [FILE]...` takes zero or more file URLs or keys. Every
`figmog_*` tool has an optional `file` argument, and a file referenced for
the first time gets mirrored then (one Tier-1 pull). Starting with zero files
needs no token up front. `figmog_open` and `figmog_files` manage the set. See
`docs/SPEC.md` §6.

### Reads shaped for building

`figmog dump <id>` returns a node and its descendants as nested raw JSON,
with `--depth N` and `--fields a,b,c` to keep the payload small.
`--under <id>` scopes `text`, `find`, `search` and `where` to one subtree,
and composes with `--page`. `--resolve-vars` annotates every
`boundVariables` binding site with the variable's name and per-mode values
under a `resolved_variables` key, so a generator can emit
`var(--color-bg-primary)` instead of a literal. Node arguments accept a bare
id, a `12-34` id, or a pasted Figma URL. See `docs/SPEC.md` §12 and §13.

### Vector geometry

`figmog pull --geometry` asks Figma for `fillGeometry` and `strokeGeometry`
path data, so vector nodes carry their outlines in `raw`. The flag is stored
per mirror, so watch ticks, `figmog_sync` and auto-opens keep requesting
geometry and the records never churn from flag drift. `pull --fresh` turns it
back off. See `docs/SPEC.md` §14.

### Image bytes

`figmog images <node-id>...` downloads node renders plus the image fills
those nodes reference, writes them to `--out` (default `./figmog-images/`),
and prints a manifest. Bytes are cached against the file version, so asking
again for an unedited file spends nothing. Nothing in figmog fetches images
on its own, because Figma's images endpoints are its most rate-limited. See
`docs/SPEC.md` §15.

### Freshness is polling, not push

`figmog serve` polls the cheap Tier-3 meta endpoint every `--interval`
seconds (default 10) and spends a Tier-1 file fetch only when the file's
watermark moves. A change shows up within one interval. Figma's `FILE_UPDATE`
webhook is Enterprise-only and debounced up to 30 minutes, so polling a cheap
endpoint beats waiting for it.

### Variables on any plan

On Enterprise, every pull also fetches `variables/local` and folds those
records in. Elsewhere figmog inverts the `boundVariables` references Figma
bakes into the file JSON, which covers each variable's default-mode value.
`figmog import-variables <export.json>` takes a full-fidelity export from
Figma's plugin console on any plan. See `docs/SPEC.md` §9.

### The tools

`figmog serve` exposes 21 `figmog_*` tools: 17 reads of the local mirror,
`figmog_sync`, `figmog_images`, and the two mirror-management tools. Unless
`--no-upstream`, it also proxies Figma's desktop Dev Mode MCP server (paid
Dev or Full seat, desktop app running), merging those tools into the same
list and caching `get_*`/`list_*` responses by file version. Every name and
argument shape is in `docs/SPEC.md` §6 and §8. `figmog tools` prints the
merged list, and `figmog call <tool>` invokes any of them from a shell.

## The benchmark run

<details>
<summary>Per-tool numbers, sync phase, and provenance</summary>

Measured 2026-08-17 on Apple Silicon against a real production file: 5,339
nodes, 5,921,203 bytes, fetched in 4,662.2ms.

Cold sync: 146.0ms flatten plus 81.3ms sync for 5,394 records, 66,364
records a second. Re-pull of the unchanged file: 4.6ms, churn zero.

| tool | calls | p50 (ms) | p95 | p99 | max |
|---|---|---|---|---|---|
| `figmog_node` | 834 | 0.055 | 0.069 | 0.082 | 1.482 |
| `figmog_search` | 834 | 0.067 | 0.124 | 0.191 | 0.621 |
| `figmog_instances` | 833 | 0.111 | 0.125 | 0.151 | 0.607 |
| `figmog_tree` | 833 | 0.462 | 0.529 | 0.733 | 2.172 |
| `figmog_stats` | 833 | 16.892 | 18.587 | 22.385 | 38.561 |
| `figmog_where` | 833 | 22.057 | 23.686 | 27.551 | 40.437 |

Load phase: 5,000 queries in 33.6s, 149 requests a second. API comparison: 5
live calls against the same file, p50 972.6ms, max 1,189.1ms. `figmog_node`
at 0.055ms against the `/nodes` p50 of 972.6ms is the 17,657x figure. At
Figma's Tier-1 budget of ~10 requests a minute, those 5,000 calls take about
500 minutes.

The harness itself was retired from the binary in v0.0.1's slim-down and
lives in the repo history: `docs/history/2026-08-16-figmog-bench.md`.

</details>

## Install

Download `figmog-<version>-<target>.tar.gz` and `SHA256SUMS` from
[Releases](https://github.com/sanctuarycomputer/figmog/releases). Targets:
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`.

```console
$ shasum -a 256 -c SHA256SUMS --ignore-missing   # sha256sum on Linux
$ tar xzf figmog-<version>-<target>.tar.gz
$ chmod +x figmog
$ xattr -d com.apple.quarantine ./figmog         # macOS: builds are unsigned
$ ./figmog --help
```

The checksum line should print `figmog-<version>-<target>.tar.gz: OK`. The
macOS builds are not signed or notarized, so Gatekeeper refuses to run them
until the quarantine attribute is cleared once. The Linux build is compiled
on Ubuntu 24.04 and needs glibc 2.39 or newer; on an older distribution,
build from source:

```console
$ git clone https://github.com/sanctuarycomputer/figmog.git
$ cd figmog && cargo build --release
```

That needs a Rust toolchain and network access, since the build fetches
`fold` from its pinned git rev.

## Limitations

- Full per-mode variable values need either an Enterprise plan or a manual
  `import-variables`; inference alone covers the default mode.
- The proxied desktop tools answer for whatever file is open in the Figma
  app. They have no concept of "which file", so a `file` argument on such a
  call is ignored.
- A file change is visible within one poll interval, not instantly.
- `figmog images` spends Figma's most rate-limited endpoints and is never
  called automatically. Cached bytes are keyed to the file version and are
  dropped when the version moves.
- Style definitions are derived from one consumer node, since the file JSON
  carries style metadata only. A style with no consumers has no derivable
  value.
- Instance overrides that Figma does not serialize into the instance's
  subtree are not reconstructed.
- `pull --fresh` wipes the whole store, including imported variables and the
  stored geometry flag.
- macOS and Linux only. The control plane is a unix domain socket and the
  code uses unix filesystem APIs directly, so there is no Windows build.
- One process may open a store at a time (fjall is single-writer).
  `figmog serve` holds that handle for its life. Other commands reach it over
  the socket instead, but `--no-socket`, or a `--db` naming the store `serve`
  owns, still gets the plain `store is locked` error.

## License

figmog is licensed under AGPL-3.0-only. The full text is in
[`LICENSE`](LICENSE). The v0.0.1 release stays MIT, for that snapshot only.
If you want figmog under other terms, email hello@sanctuary.computer and we
will talk.

figmog depends on [`fold`](https://github.com/flowercomputers/bogkit) through
a pinned git dependency, and upstream `bogkit` currently ships with no
license file. Building and running figmog from source is fine. Publishing
binaries that embed `fold` more widely than this repo's own testing releases
waits on upstream adding a license, which is a separate decision from
figmog's own license.

## Figma's terms

figmog works the way Figma's [own API guidance](https://developers.figma.com/docs/rest-api/rate-limits)
asks integrations to work: cache file data instead of refetching it, and back
off when the API says to. Every request it makes uses your token, against
official endpoints, at your seat's normal limits; the mirror exists so there
are far fewer of those requests.

## A note for Figma

You already ship a local MCP server for design agents. Shouldn't it work like
this, answering from the machine the file is already on, with no rate limit
between an agent and a design it is allowed to read?

If you would like to use this code, we would love to give it to you. Email us
at hello@sanctuary.computer.
