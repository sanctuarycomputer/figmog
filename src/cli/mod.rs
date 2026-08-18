//! Command-line surface. Read commands never touch the network: they open
//! the local store and read one snapshot.
//!
//! Split into `pull` (pull/do_pull/`PullError`/`.figmog/current` helpers),
//! `read` (the read-only query commands), and `call` (`tools`/`call`/
//! `import-variables`, the cached-proxy CLI parity surface). This module
//! keeps the clap types, top-level dispatch, `run`, and the store-opening/
//! JSON-writing helpers every submodule shares.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::ident::parse_file_ref;

mod call;
mod pull;
mod read;

pub(crate) use pull::{PullError, now_ms, pull_failure_wait, write_current};

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
        /// Annotate `boundVariables` binding sites with variable names and
        /// per-mode values under a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
    },
    /// Full raw JSON subtree dump rooted at a node (default depth: unlimited).
    Dump {
        id: String,
        #[arg(long)]
        depth: Option<usize>,
        /// Project every node to these raw fields (id/name/type/children
        /// always survive). Comma-separated, e.g. --fields id,name,fills.
        #[arg(long, value_delimiter = ',')]
        fields: Option<Vec<String>>,
        /// Annotate `boundVariables` binding sites with variable names and
        /// per-mode values under a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
    },
    /// Nodes by type, optionally within one page and/or a subtree scope.
    Find {
        #[arg(long = "type")]
        node_type: String,
        #[arg(long)]
        page: Option<String>,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
    },
    /// BM25 search over layer names and text content.
    Search {
        query: String,
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
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
        /// With --values, annotate the definition's variable bindings under
        /// a `resolved_variables` key.
        #[arg(long)]
        resolve_vars: bool,
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
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
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
        /// Scope to the subtree rooted at this node id (inclusive).
        #[arg(long)]
        under: Option<String>,
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
    use std::io::Write;
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

    // `figmog tools` never reads the store — it only needs an upstream
    // probe (spec §4 debt item M5), so it's dispatched here, before
    // `resolve_db`, exactly like `serve` above: it works with no
    // established `.figmog/current` and no `--db` at all, unlike `figmog
    // call` (below), which does need a resolved mirror to open.
    if let Cmd::Tools {
        upstream,
        no_upstream,
    } = &cli.cmd
    {
        return call::cmd_tools(upstream.clone(), *no_upstream);
    }

    let db = resolve_db(&cli)?;
    match cli.cmd {
        Cmd::Pull {
            file,
            from_file,
            fresh,
        } => pull::cmd_pull(&db, file, from_file, fresh),
        Cmd::ImportVariables { path } => call::cmd_import_variables(&db, path),
        Cmd::Call {
            tool,
            args,
            upstream,
            no_upstream,
        } => call::cmd_call(&db, tool, args, upstream, no_upstream),
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
                    read::cmd_status(&nodes, &meta)
                }),
                Cmd::Pages => st
                    .rtx(|((nodes, _, _, _, _, _, by_type), ..)| read::cmd_pages(&nodes, &by_type)),
                Cmd::Tree { id, depth } => {
                    st.rtx(|((nodes, children, _, _, _, _, by_type), ..)| {
                        read::cmd_tree(&nodes, &children, &by_type, id, depth)
                    })
                }
                Cmd::Get {
                    id,
                    children: with_children,
                    resolve_vars,
                } => st.rtx(
                    |((nodes, children, ..), _, _, _, variables, variable_collections, ..)| {
                        read::cmd_get(
                            &nodes,
                            &children,
                            &variables,
                            &variable_collections,
                            id,
                            with_children,
                            resolve_vars,
                        )
                    },
                ),
                Cmd::Dump {
                    id,
                    depth,
                    fields,
                    resolve_vars,
                } => st.rtx(
                    |((nodes, children, ..), _, _, _, variables, variable_collections, ..)| {
                        read::cmd_dump(
                            &nodes,
                            &children,
                            &variables,
                            &variable_collections,
                            id,
                            depth,
                            fields,
                            resolve_vars,
                        )
                    },
                ),
                Cmd::Find {
                    node_type,
                    page,
                    under,
                } => st.rtx(|((nodes, children, _, _, _, _, by_type), ..)| {
                    read::cmd_find(&nodes, &children, &by_type, node_type, page, under)
                }),
                Cmd::Search {
                    query,
                    limit,
                    under,
                } => st.rtx(|((nodes, children, text, ..), ..)| {
                    read::cmd_search(&nodes, &children, &text, query, limit, under)
                }),
                Cmd::Instances { target } => st.rtx(
                    |((nodes, _, _, instances_of, ..), components, component_sets, ..)| {
                        read::cmd_instances(
                            &nodes,
                            &instances_of,
                            &components,
                            &component_sets,
                            target,
                        )
                    },
                ),
                Cmd::Components => st.rtx(|((nodes, ..), components, component_sets, ..)| {
                    read::cmd_components(&component_sets, &components, &nodes)
                }),
                Cmd::Styles {
                    style_type,
                    values,
                    resolve_vars,
                } => st.rtx(
                    |(
                        (nodes, _, _, _, styled_by, ..),
                        _,
                        _,
                        styles,
                        variables,
                        variable_collections,
                        ..,
                    )| {
                        read::cmd_styles(
                            &styles,
                            &styled_by,
                            &nodes,
                            &variables,
                            &variable_collections,
                            style_type,
                            values,
                            resolve_vars,
                        )
                    },
                ),
                Cmd::Uses { id } => st.rtx(|((nodes, _, _, _, styled_by, bound_to, _), ..)| {
                    read::cmd_uses(&nodes, &styled_by, &bound_to, id)
                }),
                Cmd::Vars { id } => st.rtx(
                    |((nodes, ..), _, _, _, variables, variable_collections, _, _)| {
                        read::cmd_vars(&nodes, &variables, &variable_collections, id)
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
                        read::cmd_stats(
                            &nodes,
                            &components,
                            &component_sets,
                            &styles,
                            &variables,
                            &by_type,
                        )
                    },
                ),
                Cmd::Path { id } => st.rtx(|((nodes, ..), ..)| read::cmd_path(&nodes, id)),
                Cmd::Text { page, under } => {
                    st.rtx(|((nodes, children, _, _, _, _, by_type), ..)| {
                        read::cmd_text(&nodes, &children, &by_type, page, under)
                    })
                }
                Cmd::Where {
                    pointer,
                    equals,
                    page,
                    under,
                } => st.rtx(|((nodes, children, ..), ..)| {
                    read::cmd_where(&nodes, &children, pointer, equals, page, under)
                }),
                Cmd::At { x, y } => st.rtx(|((nodes, ..), ..)| read::cmd_at(&nodes, x, y)),
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
struct Db {
    path: PathBuf,
    key: Option<String>,
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
    // `pull::do_pull`), so a failed pull never repoints later commands.
    // `serve` never reaches here — it's handled, `Db`-free, before this
    // function is even called (see `dispatch`).
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
///
/// **Not safe to race across threads within one test binary:** the swapped
/// panic hook is process-global, not per-call — `cargo test`'s default
/// parallel test execution means a *different* test's unrelated panic,
/// happening on another thread while this function's hook is installed,
/// would also hit the no-op hook and print no banner (its own assertion
/// failure still fails that test; only the diagnostic banner is at risk).
/// This crate's own suite has never hit that in practice (the window is a
/// handful of instructions around one `catch_unwind` call), but a test
/// added here that deliberately panics on another thread concurrently
/// should account for it — e.g. by running serially (`--test-threads=1`)
/// or accepting a possibly-missing banner rather than treating one as a
/// hard requirement.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
