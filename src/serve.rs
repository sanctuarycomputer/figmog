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
//! Figma MCP an agent needs — `tools/list` merges the 20 local `figmog_*`
//! tools with every upstream tool verbatim (`proxy::merge_registry`), and
//! `tools/call` routes by the namespace rule (`proxy::is_local_tool`).
//! Upstream routing is global, not per-session: the desktop server serves
//! whatever file is open in the Figma app, independent of any mirror this
//! process manages — spec §14's documented caveat. No mid-session
//! re-probe: an unreachable upstream at startup means local-only tools
//! for the life of the process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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

// ---- unix-socket control plane (v0.0.2 spec §1) ----
//
// `figmog serve` additionally listens on `<figmog-root>/serve.sock`: a
// listener thread accepts connections, and each connection's own reader
// thread forwards newline-delimited JSON-RPC frames into the *same* `mpsc`
// channel the stdin reader already feeds — tagged with a connection id
// (`Incoming::Socket`) so the single-threaded main loop below can route
// each response back to the connection that asked for it. The store is
// still only ever touched from the main loop; socket traffic, stdio
// frames, and watch ticks all interleave through one `recv`/`recv_timeout`
// exactly as they always have.

/// One message the serve loop can receive from either its stdin reader
/// thread or a socket connection's reader thread.
#[derive(Debug)]
enum Incoming {
    /// One line of MCP stdio input.
    Stdio(String),
    /// Stdin hit EOF (or a read error): the reader thread is done and the
    /// loop should exit. An explicit sentinel — rather than relying on
    /// every `tx` clone being dropped, the pre-socket exit signal — because
    /// once socket connections hold their own long-lived `tx` clones, the
    /// channel no longer reliably disconnects just because stdin closed.
    StdinClosed,
    /// One line from socket connection `conn_id`.
    Socket(u64, String),
}

/// Registry of a socket connection's write half, keyed by connection id —
/// the main loop's only way to route a response back to the connection
/// that asked for it (its own thread only ever *reads*; writing back
/// happens from the main loop, which is the only place a `ToolHandler` may
/// touch the store). An entry is removed the moment its reader thread ends
/// (EOF or any read error — see [`spawn_socket_acceptor`]), so a client
/// that vanishes mid-request simply has no entry left to write a response
/// to by the time one's ready.
type ConnRegistry = Arc<Mutex<HashMap<u64, UnixStream>>>;

/// `<figmog_root>/serve.sock` — must match `cli::socket`'s own derivation
/// exactly, since that's how the CLI finds this process.
pub(crate) fn socket_path(figmog_root: &Path) -> PathBuf {
    figmog_root.join("serve.sock")
}

/// Outcome of probing a pre-existing socket file at startup (spec §1).
#[derive(Debug, PartialEq, Eq)]
enum StaleProbe {
    /// A connect succeeded: some other process is actively listening — this
    /// instance must not touch the file.
    Owned,
    /// A connect failed: nothing is actually listening (a leftover file
    /// from an unclean exit, or garbage that was never a socket at all) —
    /// safe to unlink and rebind.
    Stale,
}

/// Pure classification of a connect attempt against a pre-existing socket
/// path, split from the actual `UnixStream::connect` call so the decision
/// itself is unit-testable without a real socket. Spec §1 only names the
/// two outcomes that matter in practice — refused (stale) vs. success
/// (owned) — but *any* connect failure is treated as stale: a path that
/// exists but isn't a live listening socket at all (garbage left by
/// something else, or a filesystem error) means this instance can't reach
/// a live owner through it either way, so it's safe to reclaim.
fn classify_probe(connect_result: std::io::Result<()>) -> StaleProbe {
    match connect_result {
        Ok(()) => StaleProbe::Owned,
        Err(_) => StaleProbe::Stale,
    }
}

/// Generous but bounded: probing a pre-existing socket should resolve
/// near-instantly (either the connect succeeds against a live listener or
/// fails against a dead one) — this only guards against a pathological
/// hang on a socket that accepts but never proceeds.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Bind the control-plane socket at `<figmog_root>/serve.sock`, handling
/// the startup stale-probe (spec §1): no file yet ⇒ bind directly; a file
/// present ⇒ probe it via [`classify_probe`], unlinking and rebinding on
/// [`StaleProbe::Stale`] or returning the clean "another serve owns this
/// root" error on [`StaleProbe::Owned`] (`run_serve` surfaces this as an
/// ordinary `Err`, which `cli::run`'s top-level handler turns into the
/// standard `{"error": ...}` exit-1 JSON — spec §1: "exit with a clean JSON
/// error"). Sets the socket file's mode to 0600 after binding, and
/// `figmog_root` itself to 0700 (review m4): macOS in particular ignores a
/// unix-domain socket *file's* mode for connect permission checks — the
/// containing directory's mode is the real access boundary there, so
/// tightening only the socket file would be closer to cosmetic than
/// enforced on that platform. `figmog_root` may already exist (a prior
/// session's store, or a re-bind) — the mode is set unconditionally either
/// way, since every path that reaches this point is this process's own
/// root to run as its owner sees fit.
fn bind_socket(figmog_root: &Path) -> Result<(UnixListener, PathBuf), String> {
    std::fs::create_dir_all(figmog_root)
        .map_err(|e| format!("creating {}: {e}", figmog_root.display()))?;
    let mut dir_perms = std::fs::metadata(figmog_root)
        .map_err(|e| e.to_string())?
        .permissions();
    dir_perms.set_mode(0o700);
    std::fs::set_permissions(figmog_root, dir_perms).map_err(|e| e.to_string())?;

    let path = socket_path(figmog_root);

    if path.exists() {
        let connect_result = UnixStream::connect(&path).and_then(|stream| {
            stream.set_read_timeout(Some(SOCKET_PROBE_TIMEOUT))?;
            Ok(())
        });
        match classify_probe(connect_result) {
            StaleProbe::Owned => {
                return Err("another figmog serve owns this root".to_string());
            }
            StaleProbe::Stale => {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("removing stale socket {}: {e}", path.display()))?;
            }
        }
    }

    let listener =
        UnixListener::bind(&path).map_err(|e| format!("binding socket {}: {e}", path.display()))?;
    let mut perms = std::fs::metadata(&path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms).map_err(|e| e.to_string())?;
    Ok((listener, path))
}

/// Unlinks the control-plane socket file on drop — but only if the file at
/// `path` is still the *same inode* this process bound (review m3): a
/// racer, or an operator manually `rm`-ing and recreating the path while
/// this process ran, could otherwise cause this guard to delete a live
/// socket that isn't this process's own. `(dev, ino)` is recorded right
/// after a successful bind and re-checked at drop time via a fresh
/// `stat` — cheap, and the only reliable way to answer "is this still my
/// file" on a plain path (there's no unlink-by-fd primitive for a named
/// unix socket). A `stat` failure at drop time (the path is already gone)
/// is treated as nothing-to-do, not an error.
///
/// Every one of `run_serve`'s return points — a clean stdin-EOF exit, a
/// watch-loop disconnect, a future early error after the socket was bound
/// — drops this implicitly (it's held in a local binding for the rest of
/// the function's scope), so "unlinked on clean exit" (spec §1) is
/// structural rather than a set of hand-maintained cleanup calls at each
/// return site. Never constructed for a probe that found another serve
/// owning the root: [`bind_socket`] returns `Err` before creating one in
/// that case, and that socket file is not this process's to remove.
struct SocketGuard {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Result<Self, String> {
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        Ok(SocketGuard {
            path,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A single frame's hard ceiling (review m4): a client sending more than
/// this without a newline is misbehaving (or hostile) and is disconnected
/// rather than let grow this connection's read buffer without bound. Any
/// real figmog request/response — including a large `figmog_subtree` dump
/// — is orders of magnitude smaller than this.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Generous but bounded: a socket client that never reads what serve
/// writes back (accidentally or maliciously) must not be able to stall
/// this connection's response forever (review I1); well-behaved clients
/// (the CLI's own `cli::socket`, any well-formed automation) read
/// promptly and never come close to this. Set once per connection, at
/// accept time, on a handle that's `dup()`-shared with every later
/// `try_clone()` of the same connection (a unix-domain socket's send
/// timeout is a property of the underlying OS socket, not of any one
/// process-side handle to it), so [`write_socket_response`]'s later clones
/// inherit it automatically.
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Read one newline-delimited frame from `reader`, capped at
/// [`MAX_FRAME_BYTES`] (review m4). `Ok(None)` on a clean EOF with no
/// partial data; `Err` on an oversized frame (no newline found within the
/// cap) or any underlying I/O error — both mean "disconnect this
/// connection", exactly like every other read failure in
/// [`spawn_socket_acceptor`].
fn read_bounded_line<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    // Driven directly off `fill_buf`/`consume` rather than `Take` wrapping
    // `read_until`: a chunk can (and, with a client that pipelines several
    // frames back-to-back, will) contain bytes past the newline — consuming
    // only up through it, never the whole chunk, is what leaves the rest
    // buffered for the *next* call to pick up as the start of the next
    // frame, instead of dropping it.
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break; // clean EOF
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }
        buf.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
        if buf.len() > MAX_FRAME_BYTES {
            return Err(std::io::Error::other(format!(
                "frame exceeds {MAX_FRAME_BYTES} bytes with no newline"
            )));
        }
    }
    if buf.is_empty() {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Accept loop for the control-plane socket, on its own thread: each
/// connection gets its own reader thread forwarding newline-delimited
/// frames (bounded per [`read_bounded_line`]) into `tx` tagged with a
/// fresh connection id, with its write half — a non-blocking-forever send
/// timeout applied ([`SOCKET_WRITE_TIMEOUT`]) — stored in `registry` first
/// (so a request that arrives and is answered before the *next* line is
/// even read still has somewhere to route its response). An accept, clone,
/// or timeout-configuration failure for one connection is skipped rather
/// than tearing down the whole listener — a transient per-connection
/// hiccup shouldn't take the control plane down.
fn spawn_socket_acceptor(
    listener: UnixListener,
    tx: mpsc::Sender<Incoming>,
    registry: ConnRegistry,
) {
    std::thread::spawn(move || {
        let next_id = AtomicU64::new(0);
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(write_half) = stream.try_clone() else {
                continue;
            };
            if write_half
                .set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))
                .is_err()
            {
                continue;
            }
            let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
            registry.lock().unwrap().insert(conn_id, write_half);

            let tx = tx.clone();
            let registry = registry.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                loop {
                    match read_bounded_line(&mut reader) {
                        Ok(None) => break,
                        Ok(Some(l)) => {
                            if tx.send(Incoming::Socket(conn_id, l)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // The client disconnected (or sent something unreadable):
                // drop its write half so a response computed just before
                // this point (still in flight through `tx`) finds no entry
                // to write to instead of writing into a dead socket.
                registry.lock().unwrap().remove(&conn_id);
            });
        }
    });
}

/// Write one response frame back to `conn_id`'s socket connection, if it's
/// still registered — a client that vanished mid-request (its reader
/// thread already removed the entry) simply has nothing to write to,
/// matching every other "vanished client" path in this file: the response
/// is dropped, not a panic and not a wedged loop. A write failure (the
/// connection died, or [`SOCKET_WRITE_TIMEOUT`] elapsed against a
/// non-reading client) removes the now-dead entry too, so a later response
/// for the same id doesn't retry a broken pipe.
///
/// Review I1: the actual (bounded, per [`SOCKET_WRITE_TIMEOUT`]) write
/// happens on a handle cloned *out from behind* the registry lock — cloning
/// a `UnixStream` is a cheap `dup()`, not I/O — rather than while holding
/// it. Holding the lock across a blocking write would stall every other
/// connection's registry bookkeeping (a new connection's accept-time
/// `insert`, another connection's disconnect-time `remove`) behind however
/// long this one write takes to time out, on top of the (unavoidable,
/// single-threaded-loop) delay to every other queued message this causes
/// regardless.
fn write_socket_response(registry: &ConnRegistry, conn_id: u64, resp: &Value) {
    let stream = {
        let reg = registry.lock().unwrap();
        match reg.get(&conn_id).and_then(|s| s.try_clone().ok()) {
            Some(s) => s,
            None => return,
        }
    };

    let text = resp.to_string();
    let ok = writeln!(&stream, "{text}").is_ok() && (&stream).flush().is_ok();
    if !ok {
        registry.lock().unwrap().remove(&conn_id);
    }
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

/// Run the MCP stdio server (plus, unless `no_socket`, the unix-socket
/// control plane — spec §1). `db_override` is the CLI's legacy `--db
/// <path>` escape hatch (pre-v4, single-session semantics preserved
/// exactly — see `cli::dispatch`'s note on this branch); when absent,
/// `files` (zero or more, `--figmog-root`-rooted) are each mirrored at
/// startup (pulled if their store is empty and `!no_watch`), the first
/// one becoming the default. Unless `no_upstream`, also attaches Figma's
/// native desktop MCP server at `upstream_url` as a cached proxy (build
/// design §12); a failed probe degrades to local-only tools with one
/// stderr line, never a hard error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_serve(
    db_override: Option<PathBuf>,
    files: Vec<String>,
    interval: u64,
    no_watch: bool,
    upstream_url: String,
    no_upstream: bool,
    figmog_root: PathBuf,
    no_socket: bool,
) -> Result<(), String> {
    let interval_dur = clamp_interval(interval);
    let token = std::env::var("FIGMA_TOKEN").ok();

    // Bind the control-plane socket (spec §1) before anything else touches
    // the filesystem for this run: a stale-probe conflict ("another figmog
    // serve owns this root") should fail fast and unambiguously, never
    // masked by an unrelated fjall store-lock panic from a session this
    // process would otherwise go on to open first. `_socket_guard` is held
    // for the rest of this function purely for its `Drop` impl (unlinking
    // the socket file) — every return point below drops it implicitly.
    let (tx, rx) = mpsc::channel::<Incoming>();
    let registry: ConnRegistry = Arc::new(Mutex::new(HashMap::new()));
    let _socket_guard: Option<SocketGuard> = if no_socket {
        None
    } else {
        let (listener, path) = bind_socket(&figmog_root)?;
        let guard = SocketGuard::new(path)?;
        spawn_socket_acceptor(listener, tx.clone(), registry.clone());
        Some(guard)
    };

    // Reader thread: stdin lines -> mpsc, tagged `Incoming::Stdio` (see
    // `Incoming`'s doc comment for why EOF now sends an explicit
    // `StdinClosed` sentinel rather than relying on every `tx` clone being
    // dropped).
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) => {
                        if tx.send(Incoming::Stdio(l)).is_err() {
                            return;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Incoming::StdinClosed);
        });
    }

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
        "{} serving {} file(s) (watch {}, upstream {upstream_status}, socket {})",
        mcp::SERVER_NAME,
        manager.sessions.len(),
        if no_watch { "off" } else { "on" },
        if no_socket { "off" } else { "on" }
    );

    let mut next_session_idx: usize = 0;
    let mut next_deadline = Instant::now() + tick_deadline(interval_dur, manager.sessions.len());

    loop {
        let incoming = if no_watch {
            // No ticking to do, so a disconnect (all senders dropped — in
            // practice unreachable once the socket is on, since the
            // acceptor thread holds its own `tx` clone forever, but still
            // handled) is the only other thing `recv` can report besides a
            // message — exit clean rather than falling into the
            // (watch-only) timeout branch below.
            match rx.recv() {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvError) => return Ok(()),
            }
        } else {
            let wait = next_deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(msg) => Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        };

        let Some(msg) = incoming else {
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

        let line = match msg {
            Incoming::StdinClosed => return Ok(()),
            Incoming::Stdio(l) => l,
            Incoming::Socket(conn_id, l) => {
                let mut handler =
                    FnHandler(|name: &str, args: &Value| -> Result<ToolOutput, String> {
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
                if let Some(resp) = mcp::handle_message(&l, &tools, &mut handler) {
                    write_socket_response(&registry, conn_id, &resp);
                }
                continue;
            }
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
                // No user-facing geometry flag at startup (spec §4) — the
                // mirror's stored setting, if any, still drives this.
                sessions::do_pull(session, interval, false).map_err(|(message, _wait)| message)?;
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
            // No user-facing geometry flag at startup (spec §4) — the
            // mirror's stored setting, if any, still drives this.
            sessions::do_pull(session, interval, false).map_err(|(message, _wait)| message)?;
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
        // A background watch tick carries no user-facing geometry flag
        // (spec §4) — the mirror's stored setting, if any, still drives
        // this.
        Tick::Changed => match sessions::do_pull(session, interval, false) {
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
        // spec §4: `geometry` turns sticky vector-geometry requests on for
        // this mirror going forward; omitted preserves whatever's already
        // stored (default false for a brand-new mirror) — `do_pull`'s
        // stored-flag union handles that, so `false` here is exactly "no
        // new override" rather than "force off".
        let geometry = dispatch::arg_bool(args, "geometry");
        let session = manager.open(&file)?;
        let outcome = match sessions::do_pull(session, interval, geometry) {
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

    // spec §2b: an explicit `file` arg always wins, but when it's absent
    // and the call's node-id-shaped argument (`id`/`under`/`target`) is a
    // full Figma URL naming a file, that URL's file key routes the call
    // (auto-open semantics apply, same as an explicit `file`). When both
    // are present and disagree, the explicit arg still wins — the
    // disagreement is only surfaced as a note on a not-found error below,
    // never silently overridden the other way.
    let explicit_file = args.get("file").and_then(Value::as_str).map(str::to_string);
    let url_file_key = dispatch::infer_file_from_node_ref(args);
    let file_arg = explicit_file.clone().or_else(|| url_file_key.clone());
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
                // `figmog_sync` carries no geometry arg of its own (spec
                // §4) — the mirror's stored setting, if any, still drives
                // this re-pull.
                None => match sessions::do_pull(session, interval, false) {
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

        let result = (session.dispatch)(name, &call_args).map_err(|msg| {
            // spec §2b: note an explicit-file/URL-file disagreement on a
            // failed call (almost always "no node ... in the mirror" —
            // the node genuinely isn't in the file the explicit arg
            // picked) rather than staying silent about why a URL that
            // looks right didn't resolve. `explicit_file` is compared by
            // its *normalized* key, not its raw text — it can itself be a
            // full Figma URL (or a bare key) naming the very same file
            // `url_file_key` already extracted, and a raw-string compare
            // would false-positive on that (same file, different spelling).
            // A `file` arg `parse_file_ref` can't make sense of at all
            // falls back to the raw text so a genuine mismatch still shows.
            let explicit_key = explicit_file
                .as_deref()
                .map(|f| parse_file_ref(f).unwrap_or_else(|| f.to_string()));
            match (&explicit_key, &url_file_key) {
                (Some(ef), Some(uf)) if ef != uf => {
                    format!(
                        "{msg} (note: the id/under URL names file {uf}, but the explicit `file` argument {ef} was used instead)"
                    )
                }
                _ => msg,
            }
        })?;
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
        assert_eq!(tools.len(), 21);
        assert!(tools[..20].iter().all(|t| t.name.starts_with("figmog_")));
        assert_eq!(tools[20].name, "get_design_context");
        assert!(tools[20].description.starts_with("[via Figma desktop] "));
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

    // ---- unix-socket control plane (v0.0.2 spec §1) ----

    #[test]
    fn classify_probe_connect_success_means_owned() {
        assert_eq!(classify_probe(Ok(())), StaleProbe::Owned);
    }

    #[test]
    fn classify_probe_connection_refused_means_stale() {
        let err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(classify_probe(Err(err)), StaleProbe::Stale);
    }

    #[test]
    fn classify_probe_any_other_connect_error_also_means_stale() {
        // A path that exists but isn't a live listening socket at all
        // (garbage, or some other filesystem error) is treated the same
        // as a refused connection — see `classify_probe`'s doc comment.
        let err = std::io::Error::other("not a socket");
        assert_eq!(classify_probe(Err(err)), StaleProbe::Stale);
    }

    #[test]
    fn incoming_messages_carry_their_connection_tag_through_the_channel() {
        // Proves the tagging/routing mechanism itself: several sources
        // (stdin, two different socket connections) share one channel, and
        // each `Incoming` value on the receiving end still carries enough
        // information to route a response back to the right place.
        let (tx, rx) = mpsc::channel::<Incoming>();
        tx.send(Incoming::Stdio("stdio line".to_string())).unwrap();
        tx.send(Incoming::Socket(7, "first conn".to_string()))
            .unwrap();
        tx.send(Incoming::Socket(9, "second conn".to_string()))
            .unwrap();
        tx.send(Incoming::StdinClosed).unwrap();

        match rx.recv().unwrap() {
            Incoming::Stdio(l) => assert_eq!(l, "stdio line"),
            other => panic!("expected Stdio, got {other:?}"),
        }
        match rx.recv().unwrap() {
            Incoming::Socket(id, l) => {
                assert_eq!(id, 7);
                assert_eq!(l, "first conn");
            }
            other => panic!("expected Socket(7, ..), got {other:?}"),
        }
        match rx.recv().unwrap() {
            Incoming::Socket(id, l) => {
                assert_eq!(id, 9);
                assert_eq!(l, "second conn");
            }
            other => panic!("expected Socket(9, ..), got {other:?}"),
        }
        assert!(matches!(rx.recv().unwrap(), Incoming::StdinClosed));
    }

    #[test]
    fn write_socket_response_delivers_to_the_registered_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let registry: ConnRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().unwrap().insert(1, server_side);

        write_socket_response(&registry, 1, &json!({"hello": "world"}));

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v, json!({"hello": "world"}));
        // The write succeeded, so the entry is still there for a
        // subsequent response on the same connection.
        assert!(registry.lock().unwrap().contains_key(&1));
    }

    #[test]
    fn write_socket_response_for_an_unregistered_connection_is_a_silent_no_op() {
        // A response for a connection id that was never registered (or
        // whose entry was already removed — the client vanished) must not
        // panic; the response is simply dropped.
        let registry: ConnRegistry = Arc::new(Mutex::new(HashMap::new()));
        write_socket_response(&registry, 999, &json!({"ignored": true}));
        assert!(registry.lock().unwrap().is_empty());
    }

    #[test]
    fn bind_socket_creates_a_0600_file_and_a_second_probe_sees_it_as_owned() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).expect("first bind should succeed");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket file should be mode 0600");

        // A second bind attempt against the same root, with the first
        // listener still alive, must classify the existing file as owned
        // (spec §1: "success ⇒ another serve owns this root") rather than
        // stealing it.
        let err = bind_socket(&root).unwrap_err();
        assert!(err.contains("another figmog serve owns this root"), "{err}");

        drop(listener);
    }

    #[test]
    fn bind_socket_unlinks_and_rebinds_a_stale_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        // A leftover regular file at the socket path, with nothing
        // listening — the simplest reproduction of a stale socket from an
        // unclean exit.
        std::fs::write(socket_path(&root), b"not a socket").unwrap();

        let (listener, _path) = bind_socket(&root).expect("stale file should be reclaimed");
        drop(listener);
    }

    #[test]
    fn bind_socket_sets_the_root_dir_mode_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, _path) = bind_socket(&root).unwrap();
        let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "figmog-root should be tightened to 0700");
        drop(listener);
    }

    // ---- review fix round: C1/C2/m1-m6 ----

    #[test]
    fn socket_guard_does_not_unlink_a_file_replaced_at_the_same_path() {
        // Simulates a racer, or an operator manually `rm`-ing and
        // recreating the socket path while serve is still running (m3):
        // the guard must check the file is still the *inode* it bound, not
        // just that "something" exists at the path, before removing it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).unwrap();
        let guard = SocketGuard::new(path.clone()).unwrap();
        drop(listener);

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"someone else's file now").unwrap();

        drop(guard);

        assert!(
            path.exists(),
            "the guard must not remove a file it didn't itself create"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else's file now"
        );
    }

    #[test]
    fn socket_guard_unlinks_its_own_untouched_socket_file() {
        // The ordinary case, pinned alongside the "don't touch a replaced
        // file" regression above so the two behaviors stay in view
        // together.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (listener, path) = bind_socket(&root).unwrap();
        let guard = SocketGuard::new(path.clone()).unwrap();
        drop(listener);

        drop(guard);

        assert!(
            !path.exists(),
            "the guard should remove its own socket file"
        );
    }

    #[test]
    fn read_bounded_line_returns_ok_none_on_clean_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        drop(client); // EOF from the server's read side, no data ever sent.

        let mut reader = BufReader::new(server_side);
        assert_eq!(read_bounded_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn read_bounded_line_splits_multiple_pipelined_frames_from_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        // Two frames written in one `write_all` call, arriving as (very
        // likely) one chunk from the reader's perspective — proving
        // `read_bounded_line` only consumes up through the first newline,
        // leaving the second frame's bytes buffered for the next call
        // rather than dropping them.
        client.write_all(b"first\nsecond\n").unwrap();

        let mut reader = BufReader::new(server_side);
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("first".to_string())
        );
        assert_eq!(
            read_bounded_line(&mut reader).unwrap(),
            Some("second".to_string())
        );
    }

    #[test]
    fn read_bounded_line_disconnects_on_an_oversized_frame() {
        // review m4: a line with no newline that exceeds `MAX_FRAME_BYTES`
        // must error (disconnect the connection) rather than growing the
        // buffer without bound.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let oversized = vec![b'a'; MAX_FRAME_BYTES + 1];
        let writer = std::thread::spawn(move || {
            // Never completes the frame with a newline — the reader must
            // give up once the cap is exceeded, not once EOF arrives.
            let _ = client.write_all(&oversized);
        });

        let mut reader = BufReader::new(server_side);
        let result = read_bounded_line(&mut reader);
        assert!(
            result.is_err(),
            "an oversized frame should error, not block forever collecting it"
        );

        let _ = writer.join();
    }

    #[test]
    fn write_socket_response_disconnects_a_non_reading_client_within_a_bounded_time() {
        // review I1: a client that never drains what serve writes back
        // must not be able to hang `write_socket_response` (and therefore
        // the single-threaded main loop) forever. A short write timeout —
        // independent of the production `SOCKET_WRITE_TIMEOUT`, which
        // stays generous for real clients — proves the *mechanism*
        // (bounded write ⇒ disconnect ⇒ registry cleanup) without waiting
        // out the production timeout in a unit test.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap(); // never read from
        let (server_side, _) = listener.accept().unwrap();
        server_side
            .set_write_timeout(Some(Duration::from_millis(200)))
            .unwrap();

        let registry: ConnRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().unwrap().insert(1, server_side);

        // A sizeable payload, written repeatedly with nothing ever
        // draining the client's receive buffer: the OS socket buffer WILL
        // fill eventually (its exact size varies by platform, so this
        // doesn't assume a specific threshold) and a subsequent write
        // times out. Bounded overall by the loop's own deadline — if
        // `write_socket_response` ever blocked indefinitely (the pre-fix
        // behavior), this loop would hang the test past that deadline
        // instead of completing.
        let big = json!({"padding": "x".repeat(64 * 1024)});
        let start = Instant::now();
        while registry.lock().unwrap().contains_key(&1) && start.elapsed() < Duration::from_secs(10)
        {
            write_socket_response(&registry, 1, &big);
        }

        assert!(
            !registry.lock().unwrap().contains_key(&1),
            "a non-reading client's connection should eventually be dropped, not held forever"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "write_socket_response must not block indefinitely on a non-reading client"
        );
    }
}
