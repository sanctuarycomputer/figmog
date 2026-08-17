//! `figmog serve` — an MCP stdio server with the sync loop built in.
//!
//! One process owns every mirrored file's store (build design §11,
//! extended by spec §14's multi-file serve): a reader thread turns stdin
//! lines into an `mpsc` channel; the main loop owns a [`sessions::SessionManager`]
//! — one [`sessions::FileSession`] per mirrored file — and answers JSON-RPC
//! requests between poll ticks. Every local `figmog_*` tool is a thin
//! wrapper over the same `query::*` functions the CLI prints — one source
//! of truth for every answer.
//!
//! **v4 (spec §14): multi-file.** Every local tool's schema gains an
//! optional `file` argument (URL or key), routed by [`SessionManager::resolve`]
//! to the mirror it names — auto-opening it (spending one Tier-1 pull) if
//! it's new. Omitted, a tool targets the *default* file: the first one
//! given at startup, or whichever got mirrored first if none were. Two
//! new tools, `figmog_open` (mirror a file now) and `figmog_files` (list
//! every mirror), aren't per-file operations and so don't take `file`.
//! Each session owns its own store — and its own `proxy_cache` table
//! (spec §12) — opened at its own concrete `open_store!` call site inside
//! `sessions::open_session`; see that module's doc comment for why the
//! store can only ever be touched from behind one of a session's boxed
//! closures, and why that module carries a fourth (`proxy_cache`) closure
//! beyond spec §14's literal three.
//!
//! **Cache-routing choice for proxied calls (documented per spec §14's
//! caveat):** the desktop upstream has no notion of "which file" — a
//! proxied `get_*`/`list_*` call's `nodeId` could belong to any mirrored
//! file, or none of them. Rather than guess or refuse to cache, proxied
//! calls' version-keyed cache always routes through the **default**
//! session (the same one a `file`-less local tool call would answer
//! from) when at least one file is mirrored; with zero files mirrored,
//! the call is simply forwarded uncached (see [`handle_tool_call`]).
//!
//! **v3 (build design §12):** unless `--no-upstream`, figmog also probes
//! Figma's native desktop MCP server at startup and becomes the *only*
//! Figma MCP an agent needs — `tools/list` merges the 19 local `figmog_*`
//! tools with every upstream tool verbatim (`proxy::merge_registry`), and
//! `tools/call` routes by the namespace rule (`proxy::is_local_tool`).
//! Upstream routing is global, not per-session: the desktop server serves
//! whatever file is open in the Figma app, independent of any mirror this
//! process manages — spec §14's documented caveat. No mid-session
//! re-probe: an unreachable upstream at startup means local-only tools
//! for the life of the process.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::UreqApi;
use crate::dispatch;
use crate::ident::parse_file_ref;
use crate::mcp::{self, FnHandler, ToolOutput};
use crate::proxy;
use crate::sessions::{self, SessionManager};
use crate::upstream::{HttpUpstream, UpstreamMcp};
use crate::watch::Tick;

/// Default streamable-HTTP URL of Figma desktop app's Dev Mode MCP server
/// (build design §12).
pub const DEFAULT_UPSTREAM_URL: &str = "http://127.0.0.1:3845/mcp";

/// Floor for the round-robin watch tick's per-session deadline (spec §14:
/// `max(interval / session_count, 2s)`).
const MIN_TICK_DEADLINE: Duration = Duration::from_secs(2);

/// Ceiling for `--interval`, in seconds (one day) — generous for any real
/// poll cadence, but bounded: this module repeatedly adds `interval` to an
/// `Instant::now()` (directly, and via [`tick_deadline`]), and an
/// unclamped user-supplied `u64` (a typo, or an intentionally huge value)
/// risks overflowing `Instant`'s internal representation and panicking the
/// first time it's added, rather than failing gracefully.
const MAX_INTERVAL_SECS: u64 = 86_400;

/// Clamp a raw `--interval` (seconds) to [`MAX_INTERVAL_SECS`] before it's
/// ever added to an `Instant`.
fn clamp_interval(interval: u64) -> Duration {
    Duration::from_secs(interval.min(MAX_INTERVAL_SECS))
}

/// How long to wait before the next watch tick, given how many sessions
/// are being round-robin polled: the full `interval` split evenly across
/// them (so each file gets, on average, one Tier-3 poll per `interval`),
/// floored at [`MIN_TICK_DEADLINE`] so total poll spend stays bounded even
/// with many mirrored files. Zero sessions: idle at the full interval.
fn tick_deadline(interval: Duration, session_count: usize) -> Duration {
    if session_count == 0 {
        return interval;
    }
    (interval / session_count as u32).max(MIN_TICK_DEADLINE)
}

/// Run the MCP stdio server. `db_override` is the CLI's legacy `--db
/// <path>` escape hatch (pre-v4, single-session semantics preserved
/// exactly — see `cli::dispatch`'s note on this branch); when absent,
/// `files` (zero or more, `--figmog-root`-rooted) are each mirrored at
/// startup (pulled if their store is empty and `!no_watch`), the first
/// one becoming the default. Unless `no_upstream`, also attaches Figma's
/// native desktop MCP server at `upstream_url` as a cached proxy (build
/// design §12); a failed probe degrades to local-only tools with one
/// stderr line, never a hard error.
pub(crate) fn run_serve(
    db_override: Option<PathBuf>,
    files: Vec<String>,
    interval: u64,
    no_watch: bool,
    upstream_url: String,
    no_upstream: bool,
    figmog_root: PathBuf,
) -> Result<(), String> {
    let interval_dur = clamp_interval(interval);
    let token = std::env::var("FIGMA_TOKEN").ok();

    let (mut manager, track_current) = build_sessions(
        db_override,
        &files,
        no_watch,
        &token,
        &figmog_root,
        interval_dur,
    )?;

    // Upstream probe: no mid-session re-probe in v3 — an unreachable
    // desktop server at startup means local-only tools for the process's
    // whole life (build design §12). Global, not per-session (see this
    // module's doc comment).
    let mut upstream: Option<HttpUpstream> = if no_upstream {
        None
    } else {
        let mut client = HttpUpstream::new(upstream_url);
        match client.initialize() {
            Ok(()) => Some(client),
            Err(e) => {
                eprintln!("figmog: upstream unreachable, serving local tools only: {e}");
                None
            }
        }
    };
    let upstream_status: &'static str = if no_upstream {
        "disabled"
    } else if upstream.is_some() {
        "connected"
    } else {
        "unreachable"
    };

    let (tools, dropped) = match &upstream {
        Some(u) => proxy::merge_registry(dispatch::tool_registry(), u.tools()),
        None => (dispatch::tool_registry(), Vec::new()),
    };
    for name in &dropped {
        eprintln!("figmog: dropping upstream tool named like a local tool: {name}");
    }

    eprintln!(
        "{} serving {} file(s) (watch {}, upstream {upstream_status})",
        mcp::SERVER_NAME,
        manager.sessions.len(),
        if no_watch { "off" } else { "on" }
    );

    // Reader thread: stdin lines -> mpsc. EOF (or any read error) drops
    // `tx`, which is how the main loop learns to exit (`recv`/`recv_timeout`
    // return `Disconnected`).
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut next_session_idx: usize = 0;
    let mut next_deadline = Instant::now() + tick_deadline(interval_dur, manager.sessions.len());

    loop {
        let incoming = if no_watch {
            // No ticking to do, so a disconnect (stdin EOF) is the only
            // thing `recv` can report besides a line — exit clean rather
            // than falling into the (watch-only) timeout branch below.
            match rx.recv() {
                Ok(line) => Some(line),
                Err(mpsc::RecvError) => return Ok(()),
            }
        } else {
            let wait = next_deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(line) => Some(line),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        };

        let Some(line) = incoming else {
            // Timeout with watch enabled: round-robin one session's meta
            // poll, pulling inline on change (spec §14).
            next_deadline = watch_tick(
                &mut manager,
                &mut next_session_idx,
                interval_dur,
                track_current,
            );
            continue;
        };

        let mut handler = FnHandler(|name: &str, args: &Value| -> Result<ToolOutput, String> {
            handle_tool_call(
                &mut manager,
                &mut upstream,
                upstream_status,
                no_watch,
                track_current,
                interval_dur,
                &mut next_deadline,
                name,
                args,
            )
        });

        if let Some(resp) = mcp::handle_message(&line, &tools, &mut handler) {
            // `println!`/`writeln!` panic on a write error (their internal
            // `.expect`) — and the overwhelmingly likely error here is the
            // client's end of the stdio pipe having closed (it exited or
            // was killed) between our last read and this write, not a real
            // figmog fault. Write manually so that case is a clean,
            // expected shutdown (exit 0) rather than a panic or a
            // broken-pipe error message the operator can't act on.
            // Untestable cheaply at the e2e level (killing the reading end
            // of a piped subprocess mid-write isn't something
            // `assert_cmd`'s harness can trigger deterministically); this
            // code path is its own proof.
            let mut stdout = std::io::stdout();
            if writeln!(stdout, "{resp}").is_err() || stdout.flush().is_err() {
                return Ok(());
            }
        }
    }
}

/// Startup: build the [`SessionManager`] for either the legacy single-`--db`
/// path or the (possibly zero-file) multi-file path. Returns whether
/// successful pulls on the *default* session should refresh
/// `.figmog/current` (`track_current`) — only true in the no-override
/// path, matching pre-v4 behavior exactly: `--db` always resolved with no
/// established key (see `cli::resolve_db`'s old short-circuit), so it
/// never wrote `.figmog/current` either. A startup pull failure is a hard
/// error (matches old `do_pull(...)?` — the process never starts serving
/// on a file it couldn't mirror), so only the plain message half of
/// [`sessions::do_pull`]'s `(String, Duration)` failure is used here;
/// there is no retry loop yet to push a deadline out on.
fn build_sessions(
    db_override: Option<PathBuf>,
    files: &[String],
    no_watch: bool,
    token: &Option<String>,
    figmog_root: &Path,
    interval: Duration,
) -> Result<(SessionManager, bool), String> {
    if let Some(path) = db_override {
        // A `file` positional may still be given alongside `--db` (pre-v4
        // behavior: `--db` fixes the store path, an optional file arg
        // still resolves the key network operations need). With neither,
        // `network_key` stays `None` and the session's own `pull` closure
        // refuses immediately with the old clean "no file key" message —
        // `figmog_sync`/watch under `--no-watch` reach that at call time,
        // same as pre-v4 (`!no_watch` is checked here too, eagerly, so
        // watch mode still fails fast at startup rather than waiting for
        // a tick).
        let key_opt = files.first().and_then(|f| parse_file_ref(f));
        if !no_watch {
            key_opt
                .clone()
                .ok_or_else(|| "no file key: pass a file key or figma.com URL".to_string())?;
            token
                .clone()
                .ok_or_else(|| "FIGMA_TOKEN not set — required for watch".to_string())?;
        }
        let display_key = key_opt
            .clone()
            .unwrap_or_else(|| path.display().to_string());
        let session = sessions::open_session_at(
            path,
            display_key.clone(),
            key_opt.as_deref(),
            token.as_deref(),
            false,
        )?;
        let mut manager = SessionManager {
            sessions: vec![session],
            root: figmog_root.to_path_buf(),
            token: token.clone(),
            default_key: Some(display_key),
        };
        if !no_watch {
            let session = &mut manager.sessions[0];
            if !session.mirrored {
                sessions::do_pull(session, interval).map_err(|(message, _wait)| message)?;
            }
        }
        // `--db` never resolves a tracked key (pre-v4: `resolve_db`
        // short-circuited to `Db { key: None, .. }` whenever `--db` was
        // given), so it never wrote `.figmog/current` either — preserved
        // verbatim via `track_current = false` below.
        return Ok((manager, false));
    }

    // Watch needs a token to keep polling for the life of the process,
    // independent of whether any startup file actually needs an initial
    // pull — same eager requirement the old single-file path had, scaled
    // to "there's at least one file to watch" (spec §14: zero files is a
    // valid, token-free, idle startup).
    if !no_watch && !files.is_empty() {
        token
            .clone()
            .ok_or_else(|| "FIGMA_TOKEN not set — required for watch".to_string())?;
    }

    let mut manager = SessionManager {
        sessions: Vec::new(),
        root: figmog_root.to_path_buf(),
        token: token.clone(),
        default_key: None,
    };
    for (i, f) in files.iter().enumerate() {
        let session = manager.open(f)?;
        let key = session.key.clone();
        let just_pulled = if !no_watch && !session.mirrored {
            sessions::do_pull(session, interval).map_err(|(message, _wait)| message)?;
            true
        } else {
            false
        };
        if i == 0 {
            manager.default_key = Some(key.clone());
        }
        // I4: a startup pull that actually ran refreshes `.figmog/current`
        // for the default session, matching the old single-file
        // `do_pull`'s own behavior — only on a pull that happened, not on
        // every startup file regardless.
        if just_pulled {
            refresh_current(&manager, true, &key);
        }
    }
    Ok((manager, true))
}

/// One round-robin watch tick: poll exactly one session's [`Watcher`](crate::watch::Watcher)
/// and, on `Changed`, pull it via [`sessions::do_pull`] (typed-error
/// backoff, shared with every other pull call site — see that function's
/// doc comment). Returns the next deadline.
fn watch_tick(
    manager: &mut SessionManager,
    next_idx: &mut usize,
    interval: Duration,
    track_current: bool,
) -> Instant {
    if manager.sessions.is_empty() {
        return Instant::now() + interval;
    }
    // A session can only exist if opening it (an auto-open, or a startup
    // pull) already required a token — *except* a session whose own
    // startup/auto-open pull failed (sessions.rs leaves it in place,
    // `mirrored: false`, rather than evicting it — no idle eviction, spec
    // §14 non-goal; a later `resolve()` retries it). Either way, without a
    // token there's nothing safe to poll this tick.
    let Some(token) = manager.token.clone() else {
        return Instant::now() + interval;
    };

    let n = manager.sessions.len();
    let idx = *next_idx % n;
    *next_idx = (*next_idx + 1) % n;
    let deadline = tick_deadline(interval, n);

    let api = UreqApi::new(token);
    let session = &mut manager.sessions[idx];
    let key = session.key.clone();

    match session.watcher.tick(&api, &key) {
        Tick::Unchanged => Instant::now() + deadline,
        Tick::Wait { after } => Instant::now() + after,
        Tick::Changed => match sessions::do_pull(session, interval) {
            Ok(_outcome) => {
                refresh_current(manager, track_current, &key);
                Instant::now() + deadline
            }
            Err((message, wait)) => {
                eprintln!("figmog: pull failed for {key}: {message}");
                Instant::now() + wait
            }
        },
    }
}

/// Refresh `.figmog/current` to `key` — only when `track` (the no-`--db`-
/// override startup path) and `key` is the *default* session's (spec §14's
/// default rule via [`SessionManager::effective_default_key`], not merely
/// index 0), matching old single-file behavior for the one invocation
/// shape that used to do this (`figmog serve <file>`, no `--db`).
fn refresh_current(manager: &SessionManager, track: bool, key: &str) {
    if track && manager.effective_default_key().as_deref() == Some(key) {
        let _ = crate::cli::write_current(key);
    }
}

/// Route one `tools/call`. `figmog_files`/`figmog_open` aren't per-file
/// and are handled first; every other local tool's optional `file`
/// argument is extracted (and stripped before tool-specific arg parsing)
/// and routed through [`SessionManager::resolve`]; non-local names are
/// proxied — see this module's doc comment for the cache-routing choice.
/// A pull failure anywhere here (auto-open inside `resolve`, an explicit
/// `figmog_sync`, `figmog_open`) pushes `next_deadline` out by the same
/// Retry-After-aware wait [`sessions::do_pull`] computed, so a rate-limited
/// on-demand pull doesn't let the background watch loop immediately
/// re-hit the same limit for that session (build design §12, restored
/// from the pre-refactor single-session `figmog_sync` handler).
#[allow(clippy::too_many_arguments)]
fn handle_tool_call(
    manager: &mut SessionManager,
    upstream: &mut Option<HttpUpstream>,
    upstream_status: &'static str,
    no_watch: bool,
    track_current: bool,
    interval: Duration,
    next_deadline: &mut Instant,
    name: &str,
    args: &Value,
) -> Result<ToolOutput, String> {
    if name == "figmog_files" {
        return Ok(ToolOutput::Json(manager.list()));
    }

    if name == "figmog_open" {
        let file = dispatch::require_str(args, "file")?;
        let session = manager.open(&file)?;
        let outcome = match sessions::do_pull(session, interval) {
            Ok(outcome) => outcome,
            Err((message, wait)) => {
                *next_deadline = Instant::now() + wait;
                return Err(message);
            }
        };
        let key = session.key.clone();
        // The node count alone — everything else in the result comes
        // straight from `outcome`, which already has this pull's
        // authoritative name/version/churn.
        let nodes = match (session.dispatch)("figmog_status", &json!({}))? {
            ToolOutput::Json(v) => v["nodes"].clone(),
            ToolOutput::Raw(_) => Value::Null,
        };
        refresh_current(manager, track_current, &key);

        return Ok(ToolOutput::Json(json!({
            "key": key,
            "name": outcome.name,
            "version": outcome.version,
            "nodes": nodes,
            "added": outcome.churn.added,
            "changed": outcome.churn.changed,
            "removed": outcome.churn.removed,
            "unchanged": outcome.churn.unchanged,
        })));
    }

    let file_arg = args.get("file").and_then(Value::as_str).map(str::to_string);
    let mut call_args = args.clone();
    if let Some(obj) = call_args.as_object_mut() {
        obj.remove("file");
    }

    if proxy::is_local_tool(name) {
        let (session, just_pulled) =
            manager
                .resolve(file_arg.as_deref(), interval)
                .map_err(|e| {
                    if let Some(wait) = e.retry_after {
                        *next_deadline = Instant::now() + wait;
                    }
                    e.message
                })?;

        if name == "figmog_sync" {
            // `resolve` already spent this call's one pull if the session
            // was new/unmirrored — skip the redundant second Tier-1 pull
            // `figmog_sync` would otherwise always perform.
            let outcome = match just_pulled {
                Some(outcome) => outcome,
                None => match sessions::do_pull(session, interval) {
                    Ok(outcome) => outcome,
                    Err((message, wait)) => {
                        *next_deadline = Instant::now() + wait;
                        return Err(message);
                    }
                },
            };
            let key = session.key.clone();
            let churn_value = serde_json::to_value(&outcome.churn).map_err(|e| e.to_string())?;
            refresh_current(manager, track_current, &key);
            return Ok(ToolOutput::Json(churn_value));
        }

        let result = (session.dispatch)(name, &call_args)?;
        if name == "figmog_status"
            && let ToolOutput::Json(mut v) = result
        {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("upstream".to_string(), json!(upstream_status));
            }
            return Ok(ToolOutput::Json(v));
        }
        return Ok(result);
    }

    // Proxied (upstream) call: `file` carries no meaning here (spec §14's
    // documented caveat — the desktop server has no concept of "which
    // file"; a client sending it anyway is simply ignored, same as any
    // other argument the upstream tool doesn't itself define). Route the
    // version-keyed cache through the default session when one exists;
    // zero files mirrored yet means forward uncached rather than fail.
    let up = upstream
        .as_mut()
        .ok_or_else(|| format!("upstream not attached: {name}"))?;
    let (value, trigger_poll) = match manager.sessions.first_mut() {
        Some(session) => (session.proxy_cache)(up, name, args)?,
        None => {
            let result = up.call(name, args).map_err(|e| e.to_string())?;
            (result, false)
        }
    };
    if trigger_poll && !no_watch {
        *next_deadline = Instant::now();
    }
    // A proxied result is already a complete MCP `CallToolResult` from the
    // upstream — emit it verbatim (spec §11/§12; see `mcp::ToolOutput::Raw`'s
    // doc comment) rather than re-wrapping it as figmog's own text-block
    // shape.
    Ok(ToolOutput::Raw(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::ToolDef;
    use crate::upstream::FakeUpstream;

    fn local_registry() -> Vec<ToolDef> {
        dispatch::tool_registry()
    }

    #[test]
    fn merged_registry_places_local_tools_first_then_upstream_verbatim() {
        let upstream = FakeUpstream::new(vec![json!({
            "name": "get_design_context",
            "description": "Design context for a node",
            "inputSchema": {"type": "object"},
        })]);
        let (tools, dropped) = proxy::merge_registry(local_registry(), upstream.tools());
        assert!(dropped.is_empty());
        assert_eq!(tools.len(), 20);
        assert!(tools[..19].iter().all(|t| t.name.starts_with("figmog_")));
        assert_eq!(tools[19].name, "get_design_context");
        assert!(tools[19].description.starts_with("[via Figma desktop] "));
    }

    #[test]
    fn routing_local_name_never_reaches_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let mut st = crate::open_store!(&db);
        let flattened = crate::flatten::flatten_file(&json!({
            "name": "F", "version": "1", "lastModified": "t",
            "document": {"id": "0:0", "name": "Document", "type": "DOCUMENT", "children": []},
            "components": {}, "componentSets": {}, "styles": {},
        }))
        .unwrap();
        crate::store::sync(&mut st, &std::collections::BTreeSet::new(), &flattened, 0);

        let result =
            st.rtx(|r| dispatch::dispatch_read_tool("figmog_status", &json!({}), "connected", r));
        let value = result
            .expect("figmog_status is a recognized local tool")
            .unwrap();
        assert_eq!(value["upstream"], json!("connected"));

        // A non-figmog_ name is simply not recognized by the local
        // dispatcher — proving the namespace rule routes it away from the
        // local path without needing a live upstream to demonstrate it.
        let result =
            st.rtx(|r| dispatch::dispatch_read_tool("get_code", &json!({}), "connected", r));
        assert!(result.is_none());
    }

    #[test]
    fn clamp_interval_caps_at_max_and_leaves_sane_values_alone() {
        assert_eq!(clamp_interval(10), Duration::from_secs(10));
        assert_eq!(
            clamp_interval(MAX_INTERVAL_SECS),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
        // A huge/typo'd interval — including `u64::MAX`, which would
        // otherwise overflow `Instant::now() + interval` — clamps rather
        // than propagating.
        assert_eq!(
            clamp_interval(u64::MAX),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
        assert_eq!(
            clamp_interval(MAX_INTERVAL_SECS + 1),
            Duration::from_secs(MAX_INTERVAL_SECS)
        );
    }

    #[test]
    fn tick_deadline_splits_interval_across_sessions_floored_at_2s() {
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 0),
            Duration::from_secs(10)
        );
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 1),
            Duration::from_secs(10)
        );
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 5),
            Duration::from_secs(2)
        );
        // Floored: 10s / 100 sessions would be 0.1s, floored to 2s.
        assert_eq!(
            tick_deadline(Duration::from_secs(10), 100),
            Duration::from_secs(2)
        );
    }
}
