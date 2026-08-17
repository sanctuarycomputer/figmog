//! Command-line surface. Read commands never touch the network: they open
//! the local store and read one snapshot.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use fold::pipeline::terminal::{InvertedIndexReader, MultimapReader, TableReader};
use fold::stream::Readable;

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::dispatch;
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::model::{
    ComponentRec, ComponentSetRec, FileMeta, Id, NodeRec, StyleRec, VariableCollectionRec,
    VariableRec,
};
use crate::proxy;
use crate::query::{self, TextReader};
use crate::store::{Churn, collect_sweepable, collect_variable_ids, sync};
use crate::upstream::{HttpUpstream, UpstreamMcp};
use crate::watch::{BACKOFF_CAP, BACKOFF_START, Tick, Watcher};

#[derive(Parser)]
#[command(name = "figmog", about = "fold-backed local mirror of a Figma file")]
struct Cli {
    /// Emit machine-readable JSON on stdout.
    #[arg(long, global = true)]
    json: bool,
    /// Store directory (default: .figmog/<file-key>/db).
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch the file (or read a saved response) and sync the mirror.
    Pull {
        /// File key or figma.com URL. Optional after the first pull.
        file: Option<String>,
        /// Ingest a saved GET /v1/files/:key response instead of the network.
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Wipe the store and rebuild from scratch.
        #[arg(long)]
        fresh: bool,
    },
    /// Poll for changes and pull automatically.
    Watch {
        file: Option<String>,
        /// Poll interval in seconds.
        #[arg(long, default_value = "10")]
        interval: u64,
    },
    /// MCP stdio server: `figmog_*` tools over the local mirror, with the
    /// sync loop built in (one process owns the store), plus (unless
    /// `--no-upstream`) a cached proxy to Figma's native desktop MCP
    /// server — figmog is the only Figma MCP an agent needs to connect.
    Serve {
        /// File keys or figma.com URLs to mirror at startup — zero or
        /// more (spec §14: the server starts empty and mirrors files as
        /// agents reference them). The first one is the default file for
        /// any tool call that omits `file`.
        files: Vec<String>,
        /// Poll interval in seconds.
        #[arg(long, default_value = "10")]
        interval: u64,
        /// Disable the poll loop (offline/fixture use).
        #[arg(long)]
        no_watch: bool,
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// Serve local `figmog_*` tools only — no upstream proxy.
        #[arg(long)]
        no_upstream: bool,
        /// Root directory for multi-file session stores (`<root>/<key>/db`
        /// — spec §14). Hidden: testability knob so e2e tests can point
        /// startup files at pre-built fixture stores under a temp dir
        /// instead of the real `.figmog`.
        #[arg(long, default_value = ".figmog", hide = true)]
        figmog_root: PathBuf,
    },
    /// List every tool figmog would serve: the local registry, plus
    /// upstream tools when reachable.
    Tools {
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// List local `figmog_*` tools only — no upstream probe.
        #[arg(long)]
        no_upstream: bool,
    },
    /// Invoke any tool by name through the same dispatch `figmog serve`
    /// uses — local `figmog_*` tools included.
    Call {
        tool: String,
        /// JSON object of arguments (default `{}`).
        #[arg(long)]
        args: Option<String>,
        /// Figma desktop app's Dev Mode MCP server URL.
        #[arg(long, default_value = crate::serve::DEFAULT_UPSTREAM_URL)]
        upstream: String,
        /// Don't probe upstream — fail on a non-`figmog_*` tool name.
        #[arg(long)]
        no_upstream: bool,
    },
    /// File name, version, last modified, node count.
    Status,
    /// List pages.
    Pages,
    /// Subtree outline (default: whole document).
    Tree {
        id: Option<String>,
        #[arg(long)]
        depth: Option<usize>,
    },
    /// Full raw JSON of one node.
    Get {
        id: String,
        #[arg(long)]
        children: bool,
    },
    /// Nodes by type, optionally within one page.
    Find {
        #[arg(long = "type")]
        node_type: String,
        #[arg(long)]
        page: Option<String>,
    },
    /// BM25 search over layer names and text content.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
    },
    /// Instances of a component (by node id, key, or name).
    Instances { target: String },
    /// Design-system inventory: sets, variant axes, standalone components.
    Components,
    /// Styles with usage counts; --values derives definitions from consumers.
    Styles {
        #[arg(long = "type")]
        style_type: Option<String>,
        #[arg(long)]
        values: bool,
    },
    /// Nodes using a style id or bound to a variable id.
    Uses { id: String },
    /// Variables: authoritative if imported, else inferred from bindings.
    Vars { id: Option<String> },
    /// Import a variables export (REST or plugin-console shape).
    ImportVariables { path: PathBuf },
    /// Node counts by type and page, table totals, text-node count, max tree depth.
    Stats,
    /// Ancestor chain root→node for one id.
    Path { id: String },
    /// Every TEXT node's (id, characters, page_id), optionally scoped to one page.
    Text {
        #[arg(long)]
        page: Option<String>,
    },
    /// Nodes whose raw JSON matches an RFC 6901 pointer, optionally by value.
    Where {
        /// RFC 6901 pointer into the node's raw JSON, e.g. /layoutMode.
        #[arg(long)]
        pointer: String,
        /// JSON value to match; parsed as JSON, falling back to a bare string.
        #[arg(long)]
        equals: Option<String>,
        #[arg(long)]
        page: Option<String>,
    },
    /// Nodes whose absolute bounds contain a point, sorted by area ascending.
    At {
        #[arg(long)]
        x: f64,
        #[arg(long)]
        y: f64,
    },
    /// Self-contained load-test demo (build design §13): synthetic corpus
    /// (or a real file's, given one), cold sync, no-churn re-pull, and an
    /// MCP serve load test over real stdio, plus (real-file mode) a Figma
    /// API comparison — no mirror/`--db` required.
    Bench {
        /// Figma file key or figma.com URL — fetches once (one Tier-1
        /// call) and benches against the real file. Omitted: a
        /// deterministic synthetic corpus.
        file: Option<String>,
        #[arg(long, default_value = "10000")]
        nodes: usize,
        #[arg(long, default_value = "5000")]
        calls: usize,
        /// Real-file mode only: number of `GET /nodes` API-comparison calls.
        #[arg(long, default_value = "5")]
        api_calls: usize,
        /// Real-file mode only: skip the API-comparison phase entirely.
        #[arg(long)]
        skip_api: bool,
        /// Leave the temp store on disk and print its path.
        #[arg(long)]
        keep: bool,
        /// Drop into a live REPL instead of the automated phases (build
        /// design §13 "Interactive mode") — watch tool calls fire in real
        /// time. Human-only: combining with `--json` is a usage error.
        #[arg(long)]
        interactive: bool,
    },
}

/// Parse `argv`, dispatch, and return the process exit code (0 on success,
/// 1 with a one-line `figmog: <message>` on stderr otherwise).
pub fn run() -> i32 {
    let cli = Cli::parse();
    let json = cli.json;
    match dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            if json {
                eprintln!("{}", json!({"error": e}));
            } else {
                eprintln!("figmog: {e}");
            }
            1
        }
    }
}

fn dispatch(cli: Cli) -> Result<(), String> {
    // `bench` needs no mirror/db (it builds its own temp store) — handled
    // here, before `resolve_db`, exactly like the note on `open_store!`'s
    // unnameable pipeline type below explains for everything else. Matched
    // by reference so a non-match leaves `cli` untouched for the rest of
    // this function.
    if let Cmd::Bench {
        file,
        nodes,
        calls,
        api_calls,
        skip_api,
        keep,
        interactive,
    } = &cli.cmd
    {
        return cmd_bench(
            file.clone(),
            *nodes,
            *calls,
            *api_calls,
            *skip_api,
            *keep,
            *interactive,
            cli.json,
        );
    }

    // `serve` manages its own (possibly many) session stores via
    // `SessionManager` (`sessions.rs`) rather than the single `Db` every
    // other command resolves below — handled here, before `resolve_db`,
    // for the same reason `bench` is (see above): matched by reference so
    // a non-match leaves `cli` untouched for the rest of this function.
    // The global `--db` flag is still honored as a single-session escape
    // hatch (spec §14 non-goal: CLI multi-file addressing is out of
    // scope, and this keeps every pre-v4 `figmog serve --db <path>`
    // invocation — including this crate's own e2e tests — working
    // unchanged, single mirror, no `--figmog-root` layout involved).
    if let Cmd::Serve {
        files,
        interval,
        no_watch,
        upstream,
        no_upstream,
        figmog_root,
    } = &cli.cmd
    {
        return crate::serve::run_serve(
            cli.db.clone(),
            files.clone(),
            *interval,
            *no_watch,
            upstream.clone(),
            *no_upstream,
            figmog_root.clone(),
        );
    }

    let db = resolve_db(&cli)?;
    match cli.cmd {
        Cmd::Pull {
            file,
            from_file,
            fresh,
        } => cmd_pull(&db, file, from_file, fresh, cli.json),
        Cmd::Watch { file, interval } => cmd_watch(&db, file, interval, cli.json),
        Cmd::ImportVariables { path } => cmd_import_variables(&db, path, cli.json),
        Cmd::Tools {
            upstream,
            no_upstream,
        } => cmd_tools(upstream, no_upstream, cli.json),
        Cmd::Call {
            tool,
            args,
            upstream,
            no_upstream,
        } => cmd_call(&db, tool, args, upstream, no_upstream, cli.json),
        other => {
            // `open_store!`'s pipeline type contains fn items and can't be
            // named, so the store-reading dispatch below must live at this
            // concrete (non-generic) call site rather than in a helper `fn`
            // generic over `P: Push<..>` — `P::Reader<'tx, R>` would be an
            // opaque associated type there, and a tuple pattern can't
            // destructure an unconstrained associated type.
            let st = open_store_checked(|| crate::open_store!(&db.path))?;
            let json = cli.json;
            match other {
                Cmd::Status => st.rtx(|((nodes, _, _, _, _, _, _), _, _, _, _, _, meta, _)| {
                    cmd_status(&nodes, &meta, json)
                }),
                Cmd::Pages => st
                    .rtx(|((nodes, _, _, _, _, _, by_type), ..)| cmd_pages(&nodes, &by_type, json)),
                Cmd::Tree { id, depth } => {
                    st.rtx(|((nodes, children, _, _, _, _, by_type), ..)| {
                        cmd_tree(&nodes, &children, &by_type, id, depth, json)
                    })
                }
                Cmd::Get {
                    id,
                    children: with_children,
                } => st.rtx(|((nodes, children, ..), ..)| {
                    cmd_get(&nodes, &children, id, with_children, json)
                }),
                Cmd::Find { node_type, page } => st.rtx(|((nodes, _, _, _, _, _, by_type), ..)| {
                    cmd_find(&nodes, &by_type, node_type, page, json)
                }),
                Cmd::Search { query, limit } => st.rtx(|((nodes, _, text, ..), ..)| {
                    cmd_search(&nodes, &text, query, limit, json)
                }),
                Cmd::Instances { target } => st.rtx(
                    |((nodes, _, _, instances_of, ..), components, component_sets, ..)| {
                        cmd_instances(
                            &nodes,
                            &instances_of,
                            &components,
                            &component_sets,
                            target,
                            json,
                        )
                    },
                ),
                Cmd::Components => st.rtx(|((nodes, ..), components, component_sets, ..)| {
                    cmd_components(&component_sets, &components, &nodes, json)
                }),
                Cmd::Styles { style_type, values } => {
                    st.rtx(|((nodes, _, _, _, styled_by, ..), _, _, styles, ..)| {
                        cmd_styles(&styles, &styled_by, &nodes, style_type, values, json)
                    })
                }
                Cmd::Uses { id } => st.rtx(|((nodes, _, _, _, styled_by, bound_to, _), ..)| {
                    cmd_uses(&nodes, &styled_by, &bound_to, id, json)
                }),
                Cmd::Vars { id } => st.rtx(
                    |((nodes, ..), _, _, _, variables, variable_collections, _, _)| {
                        cmd_vars(&nodes, &variables, &variable_collections, id, json)
                    },
                ),
                Cmd::Stats => st.rtx(
                    |(
                        (nodes, _, _, _, _, _, by_type),
                        components,
                        component_sets,
                        styles,
                        variables,
                        ..,
                    )| {
                        cmd_stats(
                            &nodes,
                            &components,
                            &component_sets,
                            &styles,
                            &variables,
                            &by_type,
                            json,
                        )
                    },
                ),
                Cmd::Path { id } => st.rtx(|((nodes, ..), ..)| cmd_path(&nodes, id, json)),
                Cmd::Text { page } => st.rtx(|((nodes, _, _, _, _, _, by_type), ..)| {
                    cmd_text(&nodes, &by_type, page, json)
                }),
                Cmd::Where {
                    pointer,
                    equals,
                    page,
                } => st.rtx(|((nodes, ..), ..)| cmd_where(&nodes, pointer, equals, page, json)),
                Cmd::At { x, y } => st.rtx(|((nodes, ..), ..)| cmd_at(&nodes, x, y, json)),
                Cmd::Pull { .. }
                | Cmd::Watch { .. }
                | Cmd::ImportVariables { .. }
                | Cmd::Serve { .. }
                | Cmd::Tools { .. }
                | Cmd::Call { .. }
                | Cmd::Bench { .. } => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

// ---- config / db resolution ----

/// The store to open plus (when known) the file key it mirrors.
pub(crate) struct Db {
    pub(crate) path: PathBuf,
    pub(crate) key: Option<String>,
}

const CURRENT_FILE: &str = ".figmog/current";

fn resolve_db(cli: &Cli) -> Result<Db, String> {
    if let Some(path) = &cli.db {
        return Ok(Db {
            path: path.clone(),
            key: None,
        });
    }

    // pull/watch with an explicit file ref establish the key for this run.
    // `.figmog/current` is only written after a successful sync (see
    // `do_pull`), so a failed pull never repoints later commands. `serve`
    // never reaches here — it's handled, `Db`-free, before this function
    // is even called (see `dispatch`).
    if let Cmd::Pull { file: Some(f), .. } | Cmd::Watch { file: Some(f), .. } = &cli.cmd {
        let key = parse_file_ref(f).ok_or_else(|| format!("not a Figma file key or URL: {f}"))?;
        return Ok(Db {
            path: db_path_for(&key),
            key: Some(key),
        });
    }

    let key = std::fs::read_to_string(CURRENT_FILE)
        .map_err(|_| no_mirror_msg(cli))?
        .trim()
        .to_string();
    if key.is_empty() {
        return Err(no_mirror_msg(cli));
    }
    Ok(Db {
        path: db_path_for(&key),
        key: Some(key),
    })
}

/// `pull --from-file` with neither a file ref nor an established key has
/// nothing to sync into — point the user at `--from-file`'s own
/// requirements rather than the generic "run pull first" message (which
/// would tell a user already running pull to run pull).
fn no_mirror_msg(cli: &Cli) -> String {
    if let Cmd::Pull {
        from_file: Some(_),
        file: None,
        ..
    } = &cli.cmd
    {
        "--from-file needs a target mirror: pass the file key/url too, or --db <path>".into()
    } else {
        "no mirror here — run `figmog pull <file-url>` first".into()
    }
}

fn db_path_for(key: &str) -> PathBuf {
    PathBuf::from(".figmog").join(key).join("db")
}

pub(crate) fn write_current(key: &str) -> Result<(), String> {
    std::fs::create_dir_all(".figmog").map_err(|e| e.to_string())?;
    std::fs::write(CURRENT_FILE, key).map_err(|e| e.to_string())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// The clean, user-facing error every CLI store-opening call site below
/// translates a locked-store panic into (I-1). `figmog serve`/`figmog
/// watch` hold fjall's single-writer lock for the life of the process — a
/// CLI command opening the same `--db` concurrently must not surface fold's
/// raw `unwrap()` panic (exit 101).
const STORE_LOCKED_MSG: &str = "store is locked — is `figmog serve` running? Query the server instead (figmog call/tools), or stop it first.";

/// `open_store!` (via `fold::stream::Stream::new`) panics rather than
/// returning a `Result` when the underlying store can't be opened — most
/// commonly because another process (`figmog serve` or `figmog watch`)
/// already holds fjall's single-writer lock (`fjall::Error::Locked`). fold
/// itself stays untouched (its panic-on-open contract is intentional and
/// shared by `wtx`'s own rollback-on-panic path); this wrapper is figmog's
/// layer, catching that one specific panic and translating it into a clean
/// exit-1 error instead. Any *other* panic (a genuine bug — not lock
/// contention) is re-raised unchanged via `resume_unwind` so it isn't
/// silently swallowed.
///
/// Deliberately does **not** touch the global panic hook: swapping it out
/// for the call's duration would suppress *every* panic's trace, including
/// non-lock ones that get re-raised — turning a genuine bug (corrupt
/// store, disk error) into a silent exit 101 with no stderr output at all,
/// which is worse than not catching anything. The default hook stays
/// active throughout, so a lock panic still prints fold's raw trace before
/// this function's friendly `STORE_LOCKED_MSG` follows (slightly noisy,
/// but honest); swapping a process-global hook around a call is also
/// inherently racy against other threads, which this avoids entirely.
pub(crate) fn open_store_checked<T>(
    open: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> Result<T, String> {
    std::panic::catch_unwind(open).map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        if msg.contains("Locked") {
            STORE_LOCKED_MSG.to_string()
        } else {
            std::panic::resume_unwind(payload)
        }
    })
}

// ---- engine commands ----

/// Errors from [`do_pull`]: either a typed API failure (so callers can act
/// on rate limits) or any other pull-mechanics failure. `Display` matches
/// the plain-string messages `do_pull` used to produce, so `cmd_pull`'s
/// user-facing errors are unchanged.
#[derive(Debug)]
pub(crate) enum PullError {
    Api(ApiError),
    Other(String),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::Api(e) => write!(f, "{e}"),
            PullError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl From<String> for PullError {
    fn from(s: String) -> Self {
        PullError::Other(s)
    }
}

impl From<ApiError> for PullError {
    fn from(e: ApiError) -> Self {
        PullError::Api(e)
    }
}

fn cmd_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
    json: bool,
) -> Result<(), String> {
    let (churn, name, version) = do_pull(db, file, from_file, fresh).map_err(|e| e.to_string())?;
    print_churn(&churn, &name, &version, json)
}

/// The pull mechanics without any printing, so `cmd_watch` can format its
/// own per-tick event lines around the same churn. `.figmog/current` is
/// written only once the sync below has actually happened, so a failed
/// pull never repoints later commands at a nonexistent mirror.
pub(crate) fn do_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
) -> Result<(Churn, String, String), PullError> {
    // `vars_resp` is only ever `Some` on the network path — `--from-file`
    // ingests a saved `GET /v1/files/:key` response and never touches the
    // network at all, so it never calls `variables_local` either.
    let (resp, vars_resp): (Value, Option<Value>) = match from_file {
        Some(path) => {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            let resp = serde_json::from_str(&content)
                .map_err(|e| format!("parsing {}: {e}", path.display()))?;
            (resp, None)
        }
        None => {
            let key = db
                .key
                .clone()
                .or_else(|| file.and_then(|f| parse_file_ref(&f)))
                .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
            let token = std::env::var("FIGMA_TOKEN")
                .map_err(|_| "FIGMA_TOKEN not set — required for network pulls".to_string())?;
            let api = UreqApi::new(token);
            let resp = api.file(&key)?;
            // Opportunistic Enterprise variables sync (spec §12): `Ok(None)`
            // on non-Enterprise plans is not an error — v1 behavior
            // (import/inference, sweep-exempt) holds unchanged below.
            let vars_resp = api.variables_local(&key)?;
            (resp, vars_resp)
        }
    };

    if fresh {
        std::fs::remove_dir_all(&db.path).ok();
    }

    let mut flattened = flatten_file(&resp).map_err(|e| e.to_string())?;

    let mut st = open_store_checked(|| crate::open_store!(&db.path))?;
    let mut prior: BTreeSet<Id> =
        st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
            collect_sweepable(&nodes, &components, &component_sets, &styles)
        });
    if let Some(v) = &vars_resp {
        let var_recs = crate::vars::parse_variables_export(v).map_err(|e| e.to_string())?;
        flattened.recs.extend(var_recs);
        let stored_var_ids = st.rtx(|(_, _, _, _, variables, variable_collections, _, _)| {
            collect_variable_ids(&variables, &variable_collections)
        });
        prior.extend(stored_var_ids);
    }
    let prior_version =
        st.rtx(|(_, _, _, _, _, _, meta, _)| meta.get(&0).map(|m| m.version.clone()));
    let churn = sync(&mut st, &prior, &flattened, now_ms());

    // Every caller of `do_pull` (`pull`, `watch`'s per-tick pull, and
    // `figmog call figmog_sync`) goes through here, so eviction lives here
    // rather than duplicated at each call site (build design §12: a
    // version-changing pull sweeps stale `proxy_cache` rows). `figmog
    // serve`'s own pull paths don't call `do_pull` — they keep their own
    // inline eviction blocks, since they already hold `st` open and
    // re-opening it here would hit the same single-open-per-process wall
    // `figmog call figmog_sync` used to.
    if prior_version.as_deref() != Some(flattened.file.version.as_str()) {
        let stale = st.rtx(|(_, _, _, _, _, _, _, cache)| {
            crate::store::stale_cache_ids(&cache, &flattened.file.version)
        });
        if !stale.is_empty() {
            crate::store::evict_stale_cache(&mut st, &stale);
        }
    }

    if let Some(key) = &db.key {
        write_current(key)?;
    }

    Ok((
        churn,
        flattened.file.name.clone(),
        flattened.file.version.clone(),
    ))
}

fn print_churn(churn: &Churn, name: &str, version: &str, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(churn).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "synced {name} v{version}: +{} ~{} -{} (={} unchanged)",
            churn.added, churn.changed, churn.removed, churn.unchanged
        );
    }
    Ok(())
}

fn cmd_watch(db: &Db, file: Option<String>, interval: u64, json: bool) -> Result<(), String> {
    let key = db
        .key
        .clone()
        .or_else(|| file.and_then(|f| parse_file_ref(&f)))
        .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
    let token = std::env::var("FIGMA_TOKEN")
        .map_err(|_| "FIGMA_TOKEN not set — required for watch".to_string())?;
    let api = UreqApi::new(token);

    if read_watermark(db)?.is_none() {
        cmd_pull(db, Some(key.clone()), None, false, json)?;
    }

    let mut stored = read_watermark(db)?;
    let mut watcher = Watcher::new(stored.clone());
    let interval = Duration::from_secs(interval);
    // Backoff for Tier-1 pull failures, independent of the Watcher's own
    // Tier-3 meta-poll backoff — reset on any successful pull.
    let mut pull_backoff = BACKOFF_START;

    loop {
        match watcher.tick(&api, &key) {
            Tick::Unchanged => std::thread::sleep(interval),
            Tick::Wait { after } => {
                if json {
                    println!(
                        "{}",
                        json!({"event": "waiting", "seconds": after.as_secs()})
                    );
                } else {
                    println!("waiting {}s", after.as_secs());
                }
                std::thread::sleep(after);
            }
            Tick::Changed { .. } => {
                if json {
                    println!("{}", json!({"event": "changed"}));
                } else {
                    println!("changed → pulling…");
                }
                match do_pull(db, Some(key.clone()), None, false) {
                    Ok((churn, name, version)) => {
                        stored = read_watermark(db)?;
                        pull_backoff = BACKOFF_START;
                        if json {
                            let mut v = serde_json::to_value(&churn).unwrap_or_default();
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert("event".to_string(), json!("pulled"));
                            }
                            println!("{v}");
                        } else {
                            println!(
                                "synced {name} v{version}: +{} ~{} -{} (={} unchanged)",
                                churn.added, churn.changed, churn.removed, churn.unchanged
                            );
                        }
                        std::thread::sleep(interval);
                    }
                    Err(e) => {
                        eprintln!("figmog: pull failed: {e}");
                        // Watcher already advanced its watermark; reset it to
                        // the last successfully-synced one so the same
                        // change is re-detected on the next tick.
                        watcher = Watcher::new(stored.clone());
                        let wait = pull_failure_wait(&e, &mut pull_backoff, interval);
                        if json {
                            println!("{}", json!({"event": "waiting", "seconds": wait.as_secs()}));
                        } else {
                            println!("waiting {}s", wait.as_secs());
                        }
                        std::thread::sleep(wait);
                    }
                }
            }
        }
    }
}

/// How long `cmd_watch` should sleep after a failed pull, and advance the
/// per-loop backoff state. `RateLimited` honors `Retry-After` (never less
/// than the normal poll interval); anything else gets the same exponential
/// backoff discipline the [`Watcher`] uses for Tier-3 meta failures.
pub(crate) fn pull_failure_wait(
    err: &PullError,
    backoff: &mut Duration,
    interval: Duration,
) -> Duration {
    if let PullError::Api(ApiError::RateLimited { retry_after }) = err {
        interval.max(*retry_after)
    } else {
        let wait = *backoff;
        *backoff = (*backoff * 2).min(BACKOFF_CAP);
        wait
    }
}

fn cmd_import_variables(db: &Db, path: PathBuf, json: bool) -> Result<(), String> {
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let v: Value =
        serde_json::from_str(&content).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    let recs = crate::vars::parse_variables_export(&v).map_err(|e| e.to_string())?;

    let mut st = open_store_checked(|| crate::open_store!(&db.path))?;
    st.wtx(|tx| {
        for (id, rec) in &recs {
            tx.upsert(id, rec);
        }
    });

    let imported = recs
        .iter()
        .filter(|(id, _)| matches!(id, Id::Variable(_)))
        .count();
    if json {
        println!(
            "{}",
            serde_json::to_string(&json!({"imported": imported})).map_err(|e| e.to_string())?
        );
    } else {
        println!("imported {imported} variables");
    }
    Ok(())
}

/// `figmog bench [file] [--nodes N] [--calls M] [--api-calls K] [--skip-api]
/// [--keep] [--interactive]` (build design §13). Needs no resolved `Db` —
/// see `dispatch`'s early handling — so it never touches `.figmog/current`
/// or `--db`.
#[allow(clippy::too_many_arguments)]
fn cmd_bench(
    file: Option<String>,
    nodes: usize,
    calls: usize,
    api_calls: usize,
    skip_api: bool,
    keep: bool,
    interactive: bool,
    json: bool,
) -> Result<(), String> {
    if interactive && json {
        return Err(
            "--interactive is a human-only REPL and cannot be combined with --json".to_string(),
        );
    }
    let file = file
        .map(|f| parse_file_ref(&f).ok_or_else(|| format!("not a Figma file key or URL: {f}")))
        .transpose()?;
    let exe = std::env::current_exe().map_err(|e| format!("resolving current exe: {e}"))?;
    let opts = crate::bench::BenchOpts {
        nodes,
        calls,
        keep,
        exe,
        file,
        api_calls,
        skip_api,
    };
    if interactive {
        return crate::bench::run_interactive(opts);
    }
    let report = crate::bench::run(opts)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        crate::bench::print_human(&report);
    }
    Ok(())
}

pub(crate) fn read_watermark(db: &Db) -> Result<Option<String>, String> {
    let st = open_store_checked(|| crate::open_store!(&db.path))?;
    Ok(st.rtx(|(_, _, _, _, _, _, meta, _)| meta.get(&0).map(|m| m.last_modified)))
}

// ---- cached-proxy CLI parity: `figmog tools` / `figmog call` ----

/// Probe `upstream_url` unless `no_upstream`, matching `figmog serve`'s own
/// startup behavior exactly: on failure, one stderr line and local-only
/// (never a hard error — see build design §12 "Startup").
fn attach_upstream(
    upstream_url: String,
    no_upstream: bool,
) -> (Option<HttpUpstream>, &'static str) {
    if no_upstream {
        return (None, "disabled");
    }
    let mut client = HttpUpstream::new(upstream_url);
    match client.initialize() {
        Ok(()) => (Some(client), "connected"),
        Err(e) => {
            eprintln!("figmog: upstream unreachable, serving local tools only: {e}");
            (None, "unreachable")
        }
    }
}

/// `figmog tools`: the merged registry `figmog serve` would expose for this
/// mirror — local tools always, upstream tools when reachable.
fn cmd_tools(upstream_url: String, no_upstream: bool, json: bool) -> Result<(), String> {
    let (upstream, status) = attach_upstream(upstream_url, no_upstream);
    let (tools, dropped) = match &upstream {
        Some(u) => proxy::merge_registry(dispatch::tool_registry(), u.tools()),
        None => (dispatch::tool_registry(), Vec::new()),
    };
    for name in &dropped {
        eprintln!("figmog: dropping upstream tool named like a local tool: {name}");
    }

    if json {
        let rows: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "source": if proxy::is_local_tool(t.name) { "local" } else { "upstream" },
                    "cacheable": proxy::tool_name_cache_capable(t.name),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string(&rows).map_err(|e| e.to_string())?
        );
    } else {
        for t in &tools {
            let source = if proxy::is_local_tool(t.name) {
                "local"
            } else {
                "upstream"
            };
            println!(
                "{}  [{source}]  cacheable={}",
                t.name,
                proxy::tool_name_cache_capable(t.name)
            );
        }
        if status != "connected" {
            eprintln!("figmog: upstream {status} — showing local tools only");
        }
    }
    Ok(())
}

/// `figmog call <tool> [--args json]`: invoke any tool by name through the
/// same routing `figmog serve` uses — local `figmog_*` tools (including
/// `figmog_sync`) and, when attached, the upstream proxy with the same
/// cacheable-rule lookup/store.
fn cmd_call(
    db: &Db,
    tool: String,
    args: Option<String>,
    upstream_url: String,
    no_upstream: bool,
    json: bool,
) -> Result<(), String> {
    let args: Value = match args {
        Some(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("--args: invalid JSON: {e}"))?
        }
        None => json!({}),
    };

    // `figmog_sync` delegates entirely to `do_pull`, which opens its own
    // `open_store!` handle at `db.path`. fjall allows only one open handle
    // per process for a given store, so this has to return *before* this
    // function opens its own `st` below — opening both in the same process
    // deadlocks/panics on the second open's file lock (this is why this
    // branch can't just join the `if tool == "figmog_sync"` chain further
    // down, the way `figmog serve`'s handler can: `run_serve` never opens a
    // second handle for `figmog_sync`, since it reuses its own long-lived
    // `st` instead of calling `do_pull`).
    if tool == "figmog_sync" {
        let result = do_pull(db, None, None, false)
            .map(|(churn, _name, _version)| serde_json::to_value(&churn).unwrap_or_default())
            .map_err(|e| e.to_string());
        return print_call_result(result, json);
    }

    let (mut upstream, upstream_status) = attach_upstream(upstream_url, no_upstream);
    let mut st = open_store_checked(|| crate::open_store!(&db.path))?;

    let result: Result<Value, String> = if proxy::is_local_tool(&tool) {
        match st.rtx(|r| dispatch::dispatch_read_tool(&tool, &args, upstream_status, r)) {
            Some(r) => r,
            None => Err(format!("unknown tool: {tool}")),
        }
    } else {
        let up = upstream
            .as_mut()
            .ok_or_else(|| format!("upstream not attached: {tool}"))?;
        let args_canonical = proxy::canonical_args(&args);
        let version_and_hit = if proxy::is_cacheable(&tool, &args) {
            st.rtx(|(_, _, _, _, _, _, meta, cache)| {
                let version = meta.get(&0).map(|m| m.version.clone());
                let hit = version
                    .as_ref()
                    .and_then(|v| crate::cache::lookup(&cache, &tool, &args_canonical, v));
                (version, hit)
            })
        } else {
            (None, None)
        };
        proxy::proxy_call(&mut st, up, &tool, &args, version_and_hit).map(|(value, trigger_poll)| {
            if trigger_poll {
                eprintln!(
                    "figmog: {tool} may have changed the file — run `figmog pull` (or `figmog serve`, which polls automatically) to refresh the mirror"
                );
            }
            value
        })
    };

    print_call_result(result, json)
}

/// Shared `figmog call` output: pretty-printed JSON on success; on
/// failure, `{"error": ...}` on stdout (exit 0) under `--json`, otherwise
/// the plain error via the normal `figmog: <message>` / exit-1 path (see
/// `run`).
fn print_call_result(result: Result<Value, String>, json: bool) -> Result<(), String> {
    match result {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        Err(e) if json => {
            println!("{}", json!({"error": e}));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

// ---- core reads ----

fn cmd_status<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
    json: bool,
) -> Result<(), String> {
    let v = query::status(nodes, meta)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        println!(
            "{} v{} — {} nodes (last modified {})",
            v["name"].as_str().unwrap_or_default(),
            v["version"].as_str().unwrap_or_default(),
            v["nodes"].as_u64().unwrap_or_default(),
            v["last_modified"].as_str().unwrap_or_default(),
        );
    }
    Ok(())
}

fn cmd_pages<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    json: bool,
) -> Result<(), String> {
    let v = query::pages(nodes, by_type)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}",
                row["name"].as_str().unwrap_or_default(),
                row["id"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn print_tree_human(t: &query::TreeNode, indent: usize) {
    println!(
        "{}{}  [{}]  {}",
        "  ".repeat(indent),
        t.name,
        t.node_type,
        t.id
    );
    for c in &t.children {
        print_tree_human(c, indent + 1);
    }
}

fn cmd_tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
    json: bool,
) -> Result<(), String> {
    if json {
        let v = query::tree(nodes, children, by_type, id, depth)?;
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        let t = query::tree_nodes(nodes, children, by_type, id, depth)?;
        print_tree_human(&t, 0);
    }
    Ok(())
}

fn cmd_get<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    id: String,
    with_children: bool,
    _json: bool,
) -> Result<(), String> {
    let value = query::node(nodes, children, id, with_children)?;
    // Get's output is always JSON, whether or not --json was passed.
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn cmd_find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
    json: bool,
) -> Result<(), String> {
    let v = query::find(nodes, by_type, node_type, page)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  ({})",
                row["id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["page_id"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

// ---- design-system reads ----

fn cmd_search<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    text: &TextReader<'_, R>,
    query: String,
    limit: usize,
    json: bool,
) -> Result<(), String> {
    let v = query::search(text, nodes, &query, limit)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {:.3}  [{}]  {}",
                row["id"].as_str().unwrap_or_default(),
                row["score"].as_f64().unwrap_or_default(),
                row["type"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_instances<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    instances_of: &InvertedIndexReader<'_, R, String, String>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    target: String,
    json: bool,
) -> Result<(), String> {
    let v = query::instances(nodes, components, component_sets, instances_of, &target)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  ({})",
                row["id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["page_id"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_components<R: Readable>(
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    json: bool,
) -> Result<(), String> {
    let v = query::components(nodes, components, component_sets)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for s in v["sets"].as_array().into_iter().flatten() {
            println!(
                "{}  {} variants",
                s["name"].as_str().unwrap_or_default(),
                s["variants"].as_array().map(Vec::len).unwrap_or(0),
            );
        }
        for c in v["components"].as_array().into_iter().flatten() {
            println!(
                "{}  {}",
                c["node_id"].as_str().unwrap_or_default(),
                c["name"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn cmd_styles<R: Readable>(
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    style_type: Option<String>,
    values: bool,
    json: bool,
) -> Result<(), String> {
    let v = query::styles(nodes, styles, styled_by, style_type, values)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  [{}]  uses={}",
                row["style_id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["type"].as_str().unwrap_or_default(),
                row["uses"].as_u64().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_uses<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    bound_to: &InvertedIndexReader<'_, R, String, String>,
    id: String,
    json: bool,
) -> Result<(), String> {
    let v = query::uses(nodes, styled_by, bound_to, &id)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  ({})",
                row["id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["page_id"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_vars<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: Option<String>,
    json: bool,
) -> Result<(), String> {
    let v = query::vars(nodes, variables, variable_collections, id)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  [{}]  sites={}",
                row["variable_id"].as_str().unwrap_or_default(),
                row["source"].as_str().unwrap_or_default(),
                row["sites"].as_array().map(Vec::len).unwrap_or(0),
            );
        }
    }
    Ok(())
}

// ---- whole-file structural queries ----

/// `--equals <json>`: parse as JSON, falling back to treating the bare word
/// as a JSON string (so `--equals VERTICAL` works without quoting). Also
/// used by the interactive REPL's `where <pointer> [value]` shorthand
/// (`repl::parse_line`) — same fallback semantics there.
pub(crate) fn parse_equals(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn cmd_stats<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    json: bool,
) -> Result<(), String> {
    let v = query::stats(
        nodes,
        components,
        component_sets,
        styles,
        variables,
        by_type,
    )?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        println!(
            "{} nodes, max depth {}, {} text nodes",
            v["by_type"]
                .as_object()
                .map(|m| m.values().filter_map(Value::as_u64).sum::<u64>())
                .unwrap_or_default(),
            v["max_depth"].as_u64().unwrap_or_default(),
            v["text_nodes"].as_u64().unwrap_or_default(),
        );
        println!(
            "totals: components={} component_sets={} styles={} variables={}",
            v["totals"]["components"].as_u64().unwrap_or_default(),
            v["totals"]["component_sets"].as_u64().unwrap_or_default(),
            v["totals"]["styles"].as_u64().unwrap_or_default(),
            v["totals"]["variables"].as_u64().unwrap_or_default(),
        );
        println!("by type:");
        for (t, n) in v["by_type"].as_object().into_iter().flatten() {
            println!("  {t}  {n}");
        }
        println!("by page:");
        for (p, n) in v["by_page"].as_object().into_iter().flatten() {
            println!("  {p}  {n}");
        }
    }
    Ok(())
}

fn cmd_path<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    id: String,
    json: bool,
) -> Result<(), String> {
    let v = query::path(nodes, id)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  [{}]  {}",
                row["id"].as_str().unwrap_or_default(),
                row["type"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
    json: bool,
) -> Result<(), String> {
    let v = query::text(nodes, by_type, page)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  ({})  {}",
                row["id"].as_str().unwrap_or_default(),
                row["page_id"].as_str().unwrap_or_default(),
                row["characters"].as_str().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

fn cmd_where<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    pointer: String,
    equals: Option<String>,
    page: Option<String>,
    json: bool,
) -> Result<(), String> {
    let equals = equals.as_deref().map(parse_equals);
    let v = query::where_(nodes, &pointer, equals, page)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  ({})  {}",
                row["id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["page_id"].as_str().unwrap_or_default(),
                row["value"],
            );
        }
    }
    Ok(())
}

fn cmd_at<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    x: f64,
    y: f64,
    json: bool,
) -> Result<(), String> {
    let v = query::at(nodes, x, y)?;
    if json {
        println!("{}", serde_json::to_string(&v).map_err(|e| e.to_string())?);
    } else {
        for row in v.as_array().into_iter().flatten() {
            println!(
                "{}  {}  [{}]  area={}",
                row["id"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
                row["type"].as_str().unwrap_or_default(),
                row["area"].as_f64().unwrap_or_default(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I-1: a store opened a second time in-process while the first handle
    /// is still held reproduces the exact panic a CLI command hits against
    /// a running `figmog serve`/`figmog watch` (fjall's file lock conflicts
    /// on the second `File::try_lock`, regardless of whether the two opens
    /// are in the same process or different ones — see
    /// `fjall::locked_file::LockedFileGuard`). `open_store_checked` must
    /// translate that panic into the clean, exit-1-friendly message instead
    /// of letting it propagate as a raw panic.
    #[test]
    fn open_store_checked_translates_locked_store_panic_to_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");

        // Hold the first handle open, exactly like `figmog serve` does for
        // the life of its process.
        let _held = crate::open_store!(&db_path);

        let result = open_store_checked(|| crate::open_store!(&db_path));
        assert_eq!(result.err().as_deref(), Some(STORE_LOCKED_MSG));
    }

    /// The happy path: no contention, no panic, the store opens normally.
    #[test]
    fn open_store_checked_passes_through_a_successful_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        let result = open_store_checked(|| crate::open_store!(&db_path));
        assert!(result.is_ok());
    }

    /// Only the locked-store panic is translated; any other panic (a real
    /// bug, not lock contention) must propagate unchanged rather than being
    /// silently reworded into the locked-store message.
    #[test]
    fn open_store_checked_reraises_non_lock_panics_unchanged() {
        let outcome = std::panic::catch_unwind(|| open_store_checked(|| -> () { panic!("boom") }));
        let payload = outcome.expect_err("non-lock panics must still panic");
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert_eq!(msg, "boom");
    }

    #[test]
    fn rate_limited_waits_max_of_interval_and_retry_after() {
        let mut backoff = BACKOFF_START;
        let err = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(90),
        });
        // retry_after exceeds interval: use retry_after.
        let wait = pull_failure_wait(&err, &mut backoff, Duration::from_secs(10));
        assert_eq!(wait, Duration::from_secs(90));
        // rate-limit waits don't consume the exponential-backoff budget.
        assert_eq!(backoff, BACKOFF_START);

        let err = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(3),
        });
        let wait = pull_failure_wait(&err, &mut backoff, Duration::from_secs(10));
        assert_eq!(wait, Duration::from_secs(10));
    }

    #[test]
    fn other_errors_back_off_exponentially_and_cap() {
        let mut backoff = BACKOFF_START;
        let interval = Duration::from_secs(10);
        let net_err = PullError::Api(ApiError::Network("down".into()));

        let w1 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w1, Duration::from_secs(5));
        let w2 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w2, Duration::from_secs(10));
        let w3 = pull_failure_wait(&net_err, &mut backoff, interval);
        assert_eq!(w3, Duration::from_secs(20));

        // non-Api errors (e.g. flatten failures) get the same treatment.
        let other_err = PullError::Other("bad shape".into());
        let mut backoff2 = BACKOFF_CAP / 2 + Duration::from_secs(1);
        let w = pull_failure_wait(&other_err, &mut backoff2, interval);
        assert!(w <= BACKOFF_CAP);
        assert_eq!(backoff2, BACKOFF_CAP);
    }

    #[test]
    fn pull_error_display_matches_prior_stringified_messages() {
        let e = PullError::Other("FIGMA_TOKEN not set — required for network pulls".into());
        assert_eq!(
            e.to_string(),
            "FIGMA_TOKEN not set — required for network pulls"
        );

        let e = PullError::Api(ApiError::RateLimited {
            retry_after: Duration::from_secs(30),
        });
        assert_eq!(
            e.to_string(),
            ApiError::RateLimited {
                retry_after: Duration::from_secs(30)
            }
            .to_string()
        );
    }
}
