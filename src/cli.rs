//! Command-line surface. Read commands never touch the network: they open
//! the local store and read one snapshot.

use std::collections::BTreeSet;
use std::io::Write;
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
use crate::watch::BACKOFF_CAP;

#[derive(Parser)]
#[command(name = "figmog", about = "fold-backed local mirror of a Figma file")]
struct Cli {
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
        /// Poll interval in seconds. Clamped to a one-day maximum.
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
}

/// Sentinel [`dispatch`]/[`write_json`] error string meaning "stdout
/// closed mid-write, not a command failure" (the reader vanished — piped
/// into `head`, a killed consumer, …). Never printed: `run` recognizes it
/// and maps it to a silent, clean exit 0 instead of the ordinary exit-1
/// `{"error": ...}` path, the same "vanished client = clean shutdown"
/// stance `serve.rs`'s stdout frame writes take for the MCP loop.
const STDOUT_CLOSED: &str = "\0figmog:stdout-closed";

/// Parse `argv`, dispatch, and return the process exit code (0 on success,
/// 1 with `{"error": <message>}` on stderr otherwise — except a closed
/// stdout (the internal `STDOUT_CLOSED` sentinel), which is a silent 0).
pub fn run() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => 0,
        Err(e) if e == STDOUT_CLOSED => 0,
        Err(e) => {
            eprintln!("{}", json!({"error": e}));
            1
        }
    }
}

/// The one place every command writes its JSON result to stdout (spec §4:
/// JSON is the CLI's only output mode). `println!`/`print!` panic on a
/// write error (their internal `.expect`) — and the overwhelmingly likely
/// error here is the reader having gone away (piped into `head`, a killed
/// consumer, a closed terminal), not a real figmog fault. Writes manually
/// so that case becomes [`STDOUT_CLOSED`] instead: a clean, expected exit
/// 0 rather than a panic or a broken-pipe error message the operator can't
/// act on. Serialization failure (a genuine bug — `v` came from `serde_json`
/// itself, so this should never happen for real) stays a normal exit-1
/// error.
fn write_json(v: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{text}").is_err() || stdout.flush().is_err() {
        return Err(STDOUT_CLOSED.to_string());
    }
    Ok(())
}

fn dispatch(cli: Cli) -> Result<(), String> {
    // `serve` manages its own (possibly many) session stores via
    // `SessionManager` (`sessions.rs`) rather than the single `Db` every
    // other command resolves below — handled here, before `resolve_db`,
    // matched by reference so a non-match leaves `cli` untouched for the
    // rest of this function. The global `--db` flag is still honored as a
    // single-session escape hatch (spec §14 non-goal: CLI multi-file
    // addressing is out of scope, and this keeps every pre-v4 `figmog serve
    // --db <path>` invocation — including this crate's own e2e tests —
    // working unchanged, single mirror, no `--figmog-root` layout involved).
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
        } => cmd_pull(&db, file, from_file, fresh),
        Cmd::ImportVariables { path } => cmd_import_variables(&db, path),
        Cmd::Tools {
            upstream,
            no_upstream,
        } => cmd_tools(upstream, no_upstream),
        Cmd::Call {
            tool,
            args,
            upstream,
            no_upstream,
        } => cmd_call(&db, tool, args, upstream, no_upstream),
        other => {
            // `open_store!`'s pipeline type contains fn items and can't be
            // named, so the store-reading dispatch below must live at this
            // concrete (non-generic) call site rather than in a helper `fn`
            // generic over `P: Push<..>` — `P::Reader<'tx, R>` would be an
            // opaque associated type there, and a tuple pattern can't
            // destructure an unconstrained associated type.
            let st = open_store_checked(|| crate::open_store!(&db.path))?;
            match other {
                Cmd::Status => st.rtx(|((nodes, _, _, _, _, _, _), _, _, _, _, _, meta, _)| {
                    cmd_status(&nodes, &meta)
                }),
                Cmd::Pages => {
                    st.rtx(|((nodes, _, _, _, _, _, by_type), ..)| cmd_pages(&nodes, &by_type))
                }
                Cmd::Tree { id, depth } => {
                    st.rtx(|((nodes, children, _, _, _, _, by_type), ..)| {
                        cmd_tree(&nodes, &children, &by_type, id, depth)
                    })
                }
                Cmd::Get {
                    id,
                    children: with_children,
                } => st.rtx(|((nodes, children, ..), ..)| {
                    cmd_get(&nodes, &children, id, with_children)
                }),
                Cmd::Find { node_type, page } => st.rtx(|((nodes, _, _, _, _, _, by_type), ..)| {
                    cmd_find(&nodes, &by_type, node_type, page)
                }),
                Cmd::Search { query, limit } => {
                    st.rtx(|((nodes, _, text, ..), ..)| cmd_search(&nodes, &text, query, limit))
                }
                Cmd::Instances { target } => st.rtx(
                    |((nodes, _, _, instances_of, ..), components, component_sets, ..)| {
                        cmd_instances(&nodes, &instances_of, &components, &component_sets, target)
                    },
                ),
                Cmd::Components => st.rtx(|((nodes, ..), components, component_sets, ..)| {
                    cmd_components(&component_sets, &components, &nodes)
                }),
                Cmd::Styles { style_type, values } => {
                    st.rtx(|((nodes, _, _, _, styled_by, ..), _, _, styles, ..)| {
                        cmd_styles(&styles, &styled_by, &nodes, style_type, values)
                    })
                }
                Cmd::Uses { id } => st.rtx(|((nodes, _, _, _, styled_by, bound_to, _), ..)| {
                    cmd_uses(&nodes, &styled_by, &bound_to, id)
                }),
                Cmd::Vars { id } => st.rtx(
                    |((nodes, ..), _, _, _, variables, variable_collections, _, _)| {
                        cmd_vars(&nodes, &variables, &variable_collections, id)
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
                        )
                    },
                ),
                Cmd::Path { id } => st.rtx(|((nodes, ..), ..)| cmd_path(&nodes, id)),
                Cmd::Text { page } => {
                    st.rtx(|((nodes, _, _, _, _, _, by_type), ..)| cmd_text(&nodes, &by_type, page))
                }
                Cmd::Where {
                    pointer,
                    equals,
                    page,
                } => st.rtx(|((nodes, ..), ..)| cmd_where(&nodes, pointer, equals, page)),
                Cmd::At { x, y } => st.rtx(|((nodes, ..), ..)| cmd_at(&nodes, x, y)),
                Cmd::Pull { .. }
                | Cmd::ImportVariables { .. }
                | Cmd::Serve { .. }
                | Cmd::Tools { .. }
                | Cmd::Call { .. } => {
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

    // pull with an explicit file ref establishes the key for this run.
    // `.figmog/current` is only written after a successful sync (see
    // `do_pull`), so a failed pull never repoints later commands. `serve`
    // never reaches here — it's handled, `Db`-free, before this function
    // is even called (see `dispatch`).
    if let Cmd::Pull { file: Some(f), .. } = &cli.cmd {
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
/// translates a locked-store panic into (I-1). `figmog serve` holds fjall's
/// single-writer lock for the life of the process — a CLI command opening
/// the same `--db` concurrently must not surface fold's raw `unwrap()`
/// panic (exit 101).
const STORE_LOCKED_MSG: &str = "store is locked — is `figmog serve` running? Query the server instead (figmog call/tools), or stop it first.";

/// `open_store!` (via `fold::stream::Stream::new`) panics rather than
/// returning a `Result` when the underlying store can't be opened — most
/// commonly because another process (`figmog serve`) already holds fjall's
/// single-writer lock (`fjall::Error::Locked`). fold itself stays untouched
/// (its panic-on-open contract is intentional and shared by `wtx`'s own
/// rollback-on-panic path); this wrapper is figmog's layer, catching that
/// one specific panic and translating it into a clean exit-1 `Err` instead.
///
/// **stderr is JSON on every path — including a panic.** The default panic
/// hook is swapped for a no-op for the duration of the call (restored
/// immediately after, on both outcomes) so a lock panic can never print
/// fold's raw `thread 'main' panicked… ` banner ahead of our own JSON line;
/// an earlier version left the hook active specifically to preserve that
/// banner, which broke the "stderr always parses as one JSON object"
/// contract every other error path in this CLI honors. A *non*-lock panic
/// (a genuine bug — corrupt store, disk error, anything unrelated to lock
/// contention) is no longer re-raised via `resume_unwind`, since with the
/// hook suppressed that would exit 101 with **no** stderr output at all —
/// silently swallowing it is worse than the old raw-panic banner was.
/// Instead its payload message is extracted (falling back to `"unknown
/// panic"` for a non-string payload) and emitted as our own
/// `{"error": "internal panic: ..."}` JSON line, then the process exits
/// 101 directly (distinct from the ordinary exit-1 `Err` path, so a caller
/// can still tell "this command failed" from "this command's process
/// itself came apart"). One deliberate cost: this sacrifices Rust's
/// backtrace — `RUST_BACKTRACE=1` has nothing to print through a
/// suppressed hook — traded for a stderr contract every consumer (this
/// CLI's own tests included) can rely on unconditionally.
pub(crate) fn open_store_checked<T>(
    open: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> Result<T, String> {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(open);
    std::panic::set_hook(prev_hook);

    result.map_err(|payload| {
        let msg = panic_message(&*payload);
        if msg.contains("Locked") {
            STORE_LOCKED_MSG.to_string()
        } else {
            eprintln!("{}", json!({"error": format!("internal panic: {msg}")}));
            std::process::exit(101);
        }
    })
}

/// Best-effort text for a caught panic's payload: `&str`/`String` cover
/// every panic this codebase (and fold) actually raises (`panic!`,
/// `.expect(...)`, `.unwrap()`), falling back to `"unknown panic"` for
/// anything else (a non-string payload, reachable in principle via
/// `std::panic::panic_any`). Split out from [`open_store_checked`] so it's
/// unit-testable without touching that function's `std::process::exit`
/// path — which itself can't be exercised by an in-process test (calling
/// it for real would tear down the whole `cargo test` binary, not just one
/// test); that path is proven by inspection and the doc comment above.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
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
) -> Result<(), String> {
    let (churn, _name, _version) =
        do_pull(db, file, from_file, fresh).map_err(|e| e.to_string())?;
    print_churn(&churn)
}

/// The pull mechanics without any printing. `.figmog/current` is written
/// only once the sync below has actually happened, so a failed pull never
/// repoints later commands at a nonexistent mirror.
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

    // Every caller of `do_pull` (`pull` and `figmog call figmog_sync`) goes
    // through here, so eviction lives here rather than duplicated at each
    // call site (build design §12: a
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

fn print_churn(churn: &Churn) -> Result<(), String> {
    write_json(&serde_json::to_value(churn).map_err(|e| e.to_string())?)
}

/// How long a failed pull's caller (`figmog serve`'s sessions, via
/// `sessions.rs`) should wait before retrying, and advance the per-loop
/// backoff state. `RateLimited` honors `Retry-After` (never less than the
/// normal poll interval); anything else gets the same exponential backoff
/// discipline the [`Watcher`] uses for Tier-3 meta failures.
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

fn cmd_import_variables(db: &Db, path: PathBuf) -> Result<(), String> {
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
    write_json(&json!({"imported": imported}))
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
fn cmd_tools(upstream_url: String, no_upstream: bool) -> Result<(), String> {
    let (upstream, status) = attach_upstream(upstream_url, no_upstream);
    let (tools, dropped) = match &upstream {
        Some(u) => proxy::merge_registry(dispatch::tool_registry(), u.tools()),
        None => (dispatch::tool_registry(), Vec::new()),
    };
    for name in &dropped {
        eprintln!("figmog: dropping upstream tool named like a local tool: {name}");
    }
    if status != "connected" {
        eprintln!("figmog: upstream {status} — showing local tools only");
    }

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
    write_json(&Value::Array(rows))
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
        return print_call_result(result);
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
        let args_canonical = proxy::canonical_args(&args)?;
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

    print_call_result(result)
}

/// Shared `figmog call` output: pretty-printed JSON on success; on failure,
/// propagates the error so `run`'s top-level handler emits `{"error": ...}`
/// on stderr and exits 1, same as every other command.
fn print_call_result(result: Result<Value, String>) -> Result<(), String> {
    write_json(&result?)
}

// ---- core reads ----
//
// Every read command below prints its `query::*` `Value` as pretty-printed
// JSON on stdout — the only output mode (spec §4). Failures propagate as
// `Err(String)`, which `run`'s top-level handler renders as `{"error":
// ...}` on stderr with exit 1.

fn print_value(v: &Value) -> Result<(), String> {
    write_json(v)
}

fn cmd_status<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    meta: &TableReader<'_, R, u8, FileMeta>,
) -> Result<(), String> {
    print_value(&query::status(nodes, meta)?)
}

fn cmd_pages<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<(), String> {
    print_value(&query::pages(nodes, by_type)?)
}

fn cmd_tree<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    id: Option<String>,
    depth: Option<usize>,
) -> Result<(), String> {
    print_value(&query::tree(nodes, children, by_type, id, depth)?)
}

fn cmd_get<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    children: &MultimapReader<'_, R, String, (u32, String)>,
    id: String,
    with_children: bool,
) -> Result<(), String> {
    print_value(&query::node(nodes, children, id, with_children)?)
}

fn cmd_find<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    node_type: String,
    page: Option<String>,
) -> Result<(), String> {
    print_value(&query::find(nodes, by_type, node_type, page)?)
}

// ---- design-system reads ----

fn cmd_search<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    text: &TextReader<'_, R>,
    query: String,
    limit: usize,
) -> Result<(), String> {
    print_value(&query::search(text, nodes, &query, limit)?)
}

fn cmd_instances<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    instances_of: &InvertedIndexReader<'_, R, String, String>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    target: String,
) -> Result<(), String> {
    print_value(&query::instances(
        nodes,
        components,
        component_sets,
        instances_of,
        &target,
    )?)
}

fn cmd_components<R: Readable>(
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    nodes: &TableReader<'_, R, String, NodeRec>,
) -> Result<(), String> {
    print_value(&query::components(nodes, components, component_sets)?)
}

fn cmd_styles<R: Readable>(
    styles: &TableReader<'_, R, String, StyleRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    nodes: &TableReader<'_, R, String, NodeRec>,
    style_type: Option<String>,
    values: bool,
) -> Result<(), String> {
    print_value(&query::styles(
        nodes, styles, styled_by, style_type, values,
    )?)
}

fn cmd_uses<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    styled_by: &InvertedIndexReader<'_, R, String, String>,
    bound_to: &InvertedIndexReader<'_, R, String, String>,
    id: String,
) -> Result<(), String> {
    print_value(&query::uses(nodes, styled_by, bound_to, &id)?)
}

fn cmd_vars<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    variable_collections: &TableReader<'_, R, String, VariableCollectionRec>,
    id: Option<String>,
) -> Result<(), String> {
    print_value(&query::vars(nodes, variables, variable_collections, id)?)
}

// ---- whole-file structural queries ----

/// `--equals <json>`: parse as JSON, falling back to treating the bare word
/// as a JSON string (so `--equals VERTICAL` works without quoting).
pub(crate) fn parse_equals(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn cmd_stats<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    components: &TableReader<'_, R, String, ComponentRec>,
    component_sets: &TableReader<'_, R, String, ComponentSetRec>,
    styles: &TableReader<'_, R, String, StyleRec>,
    variables: &TableReader<'_, R, String, VariableRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
) -> Result<(), String> {
    print_value(&query::stats(
        nodes,
        components,
        component_sets,
        styles,
        variables,
        by_type,
    )?)
}

fn cmd_path<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    id: String,
) -> Result<(), String> {
    print_value(&query::path(nodes, id)?)
}

fn cmd_text<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    by_type: &InvertedIndexReader<'_, R, String, String>,
    page: Option<String>,
) -> Result<(), String> {
    print_value(&query::text(nodes, by_type, page)?)
}

fn cmd_where<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    pointer: String,
    equals: Option<String>,
    page: Option<String>,
) -> Result<(), String> {
    let equals = equals.as_deref().map(parse_equals);
    print_value(&query::where_(nodes, &pointer, equals, page)?)
}

fn cmd_at<R: Readable>(
    nodes: &TableReader<'_, R, String, NodeRec>,
    x: f64,
    y: f64,
) -> Result<(), String> {
    print_value(&query::at(nodes, x, y)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::BACKOFF_START;

    /// I-1: a store opened a second time in-process while the first handle
    /// is still held reproduces the exact panic a CLI command hits against
    /// a running `figmog serve` (fjall's file lock conflicts
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

    /// `panic_message` is what tells a lock panic (reworded to
    /// `STORE_LOCKED_MSG`) apart from any other panic (emitted verbatim as
    /// `internal panic: <msg>` and exit 101 — see `open_store_checked`'s
    /// doc comment) — covering the `&str` and `String` payload shapes a
    /// real `panic!`/`.unwrap()` produces, plus the `"unknown panic"`
    /// fallback for a payload that's neither.
    #[test]
    fn panic_message_extracts_str_and_string_payloads_and_falls_back() {
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*str_payload), "boom");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
        assert_eq!(panic_message(&*string_payload), "boom");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(panic_message(&*other_payload), "unknown panic");
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
