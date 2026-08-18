//! Multi-file serve (spec §14): each mirrored Figma file is a [`FileSession`]
//! — its own store, opened at its own `open_store!` call site, with
//! everything that touches it captured in boxed closures (`dispatch`,
//! `pull`, `watermark`, `proxy_cache` — four, not three; see
//! [`FileSession`]'s own doc comment for why a fourth closure was
//! necessary beyond spec §14's literal three). This generalizes the
//! single-store logic `serve.rs` used to inline directly: the store's
//! pipeline type contains fn items and can't be named (see the doc
//! comment on `store.rs`'s `open_store!` macro and the identical note in
//! `cli::dispatch`), so a session's closures share the one open handle via
//! `Rc<RefCell<_>>` — the only way several independently-boxed `FnMut`s
//! can all reach the same unnameable, non-`Copy` value.
//!
//! [`SessionManager`] owns every open session, in open order, and
//! implements the `file`-argument routing rule (spec §14): explicit
//! `file` → that mirror, auto-opening (and spending exactly one Tier-1
//! pull, with backoff/retry — see [`do_pull`]) if it's new *or was never
//! successfully mirrored*; omitted → the first **startup** FILE if one
//! was given, else the single mirrored file if exactly one exists, else
//! an error naming `figmog_open`/`figmog_files`. `default_key` carries
//! the startup-established default independently of open order, so a
//! later auto-open (or two) never silently becomes "the default" the way
//! plain first-in-`Vec` order would.
//!
//! **Proxied (upstream) tools stay outside this module entirely** — spec
//! §14's documented caveat is that the desktop server has no concept of
//! "which file", so the `file` argument only ever routes figmog's own
//! local tools. `serve.rs` still owns the single, global upstream
//! connection and decides for itself which session's `proxy_cache` a
//! proxied call reads/writes through (see its own doc comment for that
//! choice).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{FigmaApi, UreqApi};
use crate::cli::{PullError, now_ms, open_store_checked, pull_failure_wait};
use crate::dispatch;
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::mcp::ToolOutput;
use crate::model::Id;
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids};
use crate::watch::{BACKOFF_START, Watcher};

/// What one [`FileSession::pull`] call did: the sync churn plus the file's
/// name/version as of that pull — `figmog_open`'s own result and
/// `figmog_sync`'s churn value are both built from this.
#[derive(Debug)]
pub(crate) struct PullOutcome {
    pub(crate) churn: Churn,
    pub(crate) name: String,
    pub(crate) version: String,
}

// Named aliases for `FileSession`'s boxed-closure fields (clippy's
// `type_complexity` lint, and plain readability): every one of these
// exists only because the store's pipeline type contains fn items and
// can't be named (this module's doc comment), so it can never appear in a
// named struct field directly — only behind one of these closures.
type DispatchFn = Box<dyn FnMut(&str, &Value) -> Result<ToolOutput, String>>;
/// `bool` argument: this pull's own geometry override (spec §4) —
/// `figmog_open`'s `geometry` arg, or `false` from every other call site
/// (startup, watch tick, `figmog_sync`, auto-open), which don't carry a
/// user-facing override of their own and so just let the mirror's already-
/// stored setting (if any) keep driving the request — see [`do_pull`].
type PullFn = Box<dyn FnMut(bool) -> Result<PullOutcome, PullError>>;
type WatermarkFn = Box<dyn FnMut() -> Option<String>>;
type ProxyCacheFn = Box<
    dyn FnMut(&mut crate::upstream::HttpUpstream, &str, &Value) -> Result<(Value, bool), String>,
>;

/// One mirrored Figma file. `dispatch` answers the 16 read-only
/// `figmog_*` tools (everything [`dispatch::dispatch_read_tool`] knows);
/// `pull` runs one Tier-1 fetch-flatten-sync-evict cycle (used by
/// startup, `figmog_sync`, `figmog_open`, a watch tick's `Changed` branch,
/// and [`SessionManager::resolve`]'s auto-open) and returns the *typed*
/// [`PullError`] on failure — not a plain string — so every call site can
/// apply the same Retry-After-aware backoff `cli::pull_failure_wait`
/// gives the CLI's own `pull`/`watch` commands (see [`do_pull`], the
/// shared helper every one of those call sites goes through); `watermark`
/// reads the stored `FileMeta.last_modified`, used to (re)seed `watcher`
/// after every successful pull. `mirrored`: whether this session has ever
/// completed a successful pull — `false` right after a *failed* auto-open
/// (a session that pushed into `SessionManager::sessions` before its pull
/// could succeed must not be treated as "already mirrored" forever, or a
/// transient failure would silently poison the key into permanent empty
/// results — see [`SessionManager::resolve`]). `watcher`/`backoff` are
/// plain per-session state (not closures) so `serve.rs`'s round-robin tick
/// loop can drive them directly, exactly as the old single-session loop
/// drove its own local variables.
///
/// `proxy_cache` is a fourth closure beyond spec §14's literal list,
/// needed for the same structural reason as the other three: a proxied
/// (upstream, non-`figmog_*`) call's version-keyed cache lives in *this*
/// session's own `proxy_cache` table (spec §12/§14: "each session's store
/// carries its own `proxy_cache`"), and the store can only ever be touched
/// from behind one of these closures. `serve.rs` calls it only for the
/// *default* session — see its own doc comment for why proxied calls
/// route through the default rather than any per-call `file` argument.
pub(crate) struct FileSession {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) dispatch: DispatchFn,
    pub(crate) pull: PullFn,
    pub(crate) watermark: WatermarkFn,
    pub(crate) proxy_cache: ProxyCacheFn,
    pub(crate) watcher: Watcher,
    pub(crate) backoff: Duration,
    pub(crate) mirrored: bool,
}

impl FileSession {
    /// Reseed `watcher`/`backoff`/`mirrored` after a successful pull from
    /// any call site (startup, `figmog_sync`, `figmog_open`, a watch
    /// tick, auto-open) — the same bookkeeping every one of those paths
    /// used to repeat inline.
    pub(crate) fn note_pull_success(&mut self, outcome: &PullOutcome) {
        self.name = outcome.name.clone();
        let seen = (self.watermark)();
        self.watcher = Watcher::new(seen);
        self.backoff = BACKOFF_START;
        self.mirrored = true;
    }
}

/// Run `session.pull()` and apply the same failure-backoff discipline
/// `cli::pull_failure_wait` gives (Retry-After honored for a rate limit,
/// exponential otherwise, capped): on success, reseed the session via
/// [`FileSession::note_pull_success`]; on failure, reset `watcher` to the
/// last successfully-synced watermark (so the same change is re-detected
/// next time) and advance
/// `backoff`, returning the resulting wait alongside the stringified
/// error so every call site (watch tick, `figmog_sync`, `figmog_open`,
/// `SessionManager::resolve`'s auto-open) can push its own `next_deadline`
/// out by the same amount — spec §14 "backoff discipline per session".
/// Every one of those call sites shares this single implementation so
/// "same treatment" is structural, not a convention to keep in sync by
/// hand.
pub(crate) fn do_pull(
    session: &mut FileSession,
    interval: Duration,
    geometry: bool,
) -> Result<PullOutcome, (String, Duration)> {
    match (session.pull)(geometry) {
        Ok(outcome) => {
            session.note_pull_success(&outcome);
            Ok(outcome)
        }
        Err(e) => {
            session.watcher = Watcher::new((session.watermark)());
            let wait = pull_failure_wait(&e, &mut session.backoff, interval);
            Err((e.to_string(), wait))
        }
    }
}

/// Build a [`FileSession`] mirroring `key` under `root` (`root/<key>/db` —
/// the per-key store layout every mirror has used since v1, see
/// `cli::db_path_for`). Thin wrapper over [`open_session_at`] with
/// `network_key = Some(key)` — every session built this way (startup
/// files, auto-open) always has a real, parsed Figma key to pull with.
pub(crate) fn open_session(
    root: &Path,
    key: &str,
    api_token: Option<&str>,
    pull_now: bool,
) -> Result<FileSession, String> {
    let path = root.join(key).join("db");
    open_session_at(path, key.to_string(), Some(key), api_token, pull_now)
}

/// Like [`open_session`], but at an explicit store path rather than one
/// derived from `root`/`key`, and with the Figma key used for *network*
/// calls tracked separately from `key` (the session's display/dedupe
/// identity). The CLI's legacy `--db <path>` escape hatch (`serve.rs`'s
/// `run_serve`) needs both: it predates multi-file serve and its existing
/// tests pin an explicit, arbitrary store directory (no `--figmog-root`
/// layout involved), so `figmog serve --db <path>` keeps opening exactly
/// that path as a single session, unchanged — and when no file ref was
/// given alongside `--db` either, that session has no real Figma key at
/// all. `network_key: None` is exactly that case: the built session still
/// dedupes/displays under a path-derived sentinel `key`, but any attempt
/// to pull it fails immediately with the same clean message pre-v4
/// `figmog serve`/`do_pull` always gave ("no file key: pass a file key or
/// figma.com URL") instead of trying — and failing confusingly — to fetch
/// a "file" named after a filesystem path.
pub(crate) fn open_session_at(
    path: PathBuf,
    key: String,
    network_key: Option<&str>,
    api_token: Option<&str>,
    pull_now: bool,
) -> Result<FileSession, String> {
    let token = api_token.map(str::to_string);
    let network_key = network_key.map(str::to_string);
    let st = Rc::new(RefCell::new(open_store_checked(|| {
        crate::open_store!(&path)
    })?));

    // The pull-and-sync cycle (build design §12's do_pull-equivalent
    // sequence): fetch, flatten, sync, evict stale cache rows on a version
    // change. Defined once and reused for the immediate `pull_now` call
    // below and — moved as-is — for the `pull` closure every other call
    // site (`figmog_sync`, `figmog_open`, watch, auto-open) drives via
    // [`do_pull`].
    let pull_closure = {
        let st = st.clone();
        let network_key = network_key.clone();
        let token = token.clone();
        move |geometry_override: bool| -> Result<PullOutcome, PullError> {
            let key = network_key.clone().ok_or_else(|| {
                PullError::from("no file key: pass a file key or figma.com URL".to_string())
            })?;
            let token = token.clone().ok_or_else(|| {
                PullError::from("FIGMA_TOKEN not set — required for network pulls".to_string())
            })?;
            // Sticky vector geometry (spec §4): what this pull requests
            // combines its own override (`figmog_open`'s `geometry` arg,
            // or `false` from every other caller — see `PullFn`'s doc
            // comment) with whatever's already stored, so a plain re-pull
            // of a mirror that was ever opened with `--geometry`/
            // `geometry: true` keeps asking for it.
            let stored_geometry = st
                .borrow()
                .rtx(|(.., mirror_config)| store::read_geometry(&mirror_config));
            let request_geometry = store::effective_geometry(geometry_override, stored_geometry);
            let api = UreqApi::new(token);
            let resp = api.file(&key, request_geometry)?;
            // Opportunistic Enterprise variables sync (spec §12): `Ok(None)`
            // on non-Enterprise plans is not an error — v1 behavior
            // (import/inference, sweep-exempt) holds unchanged below.
            let vars_resp = api.variables_local(&key)?;
            let mut flattened = flatten_file(&resp).map_err(|e| e.to_string())?;

            let mut st = st.borrow_mut();
            let mut prior: BTreeSet<Id> =
                st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
                    collect_sweepable(&nodes, &components, &component_sets, &styles)
                });
            if let Some(v) = &vars_resp {
                let var_recs = crate::vars::parse_variables_export(v).map_err(|e| e.to_string())?;
                flattened.recs.extend(var_recs);
                let stored_var_ids =
                    st.rtx(|(_, _, _, _, variables, variable_collections, _, _, _)| {
                        collect_variable_ids(&variables, &variable_collections)
                    });
                prior.extend(stored_var_ids);
            }
            let churn = store::sync(&mut st, &prior, &flattened, now_ms());

            // Cache eviction lives here rather than folded into `sync`
            // (store.rs's own note): a version-changing pull sweeps stale
            // `proxy_cache` rows; computing `stale` against the version
            // just synced makes this a no-op whenever the version didn't
            // actually move, with no separate "did it change" check needed.
            let version = flattened.file.version.clone();
            let stale =
                st.rtx(|(_, _, _, _, _, _, _, cache, _)| store::stale_cache_ids(&cache, &version));
            if !stale.is_empty() {
                store::evict_stale_cache(&mut st, &stale);
            }

            // Persist the (possibly just-turned-on) geometry flag so the
            // *next* pull's own peek above sees it, even if this call's
            // override was the only reason it was requested this time.
            store::upsert_mirror_config(&mut st, request_geometry);

            Ok(PullOutcome {
                churn,
                name: flattened.file.name.clone(),
                version,
            })
        }
    };

    if pull_now {
        pull_closure(false).map_err(|e| e.to_string())?;
    }

    let meta_present = st
        .borrow()
        .rtx(|(_, _, _, _, _, _, meta, _, _)| meta.get(&0).is_some());
    let name = st
        .borrow()
        .rtx(|(_, _, _, _, _, _, meta, _, _)| meta.get(&0).map(|m| m.name.clone()))
        .unwrap_or_else(|| key.clone());
    let seen = st
        .borrow()
        .rtx(|(_, _, _, _, _, _, meta, _, _)| meta.get(&0).map(|m| m.last_modified.clone()));

    let dispatch: DispatchFn = {
        let st = st.clone();
        Box::new(
            move |name: &str, args: &Value| -> Result<ToolOutput, String> {
                let st = st.borrow();
                // `upstream_status` is a purely global concern (proxy routing
                // never varies per file — see this module's doc comment), so
                // it's spliced into a client-requested `figmog_status` result
                // at the call site that actually knows it (`serve.rs`), not
                // here. `""` is never observed by a real caller.
                match st.rtx(|r| dispatch::dispatch_read_tool(name, args, "", r)) {
                    Some(result) => result.map(ToolOutput::Json),
                    None => Err(format!("unknown tool: {name}")),
                }
            },
        )
    };

    let pull: PullFn = Box::new(pull_closure);

    let watermark: WatermarkFn = {
        let st = st.clone();
        Box::new(move || {
            st.borrow()
                .rtx(|(_, _, _, _, _, _, meta, _, _)| meta.get(&0).map(|m| m.last_modified.clone()))
        })
    };

    let proxy_cache: ProxyCacheFn = {
        let st = st.clone();
        Box::new(move |upstream, name, args| {
            let args_canonical = crate::proxy::canonical_args(args)?;
            let version_and_hit = if crate::proxy::is_cacheable(name, args) {
                st.borrow().rtx(|(_, _, _, _, _, _, meta, cache, _)| {
                    let version = meta.get(&0).map(|m| m.version.clone());
                    let hit = version
                        .as_ref()
                        .and_then(|v| crate::cache::lookup(&cache, name, &args_canonical, v));
                    (version, hit)
                })
            } else {
                (None, None)
            };
            let mut st = st.borrow_mut();
            crate::proxy::proxy_call(&mut st, upstream, name, args, version_and_hit)
        })
    };

    Ok(FileSession {
        key,
        name,
        dispatch,
        pull,
        watermark,
        proxy_cache,
        watcher: Watcher::new(seen),
        backoff: BACKOFF_START,
        mirrored: meta_present,
    })
}

/// Every mirrored file for one `figmog serve` process, in open order.
/// `root`/`token` are what every auto-opened session is built with
/// ([`open_session`]). `default_key`: the startup-established default (spec
/// §14's "first startup FILE"), set once by `serve.rs`'s startup
/// orchestration and never mutated by a later auto-open — see this
/// module's doc comment and [`SessionManager::resolve`].
pub(crate) struct SessionManager {
    pub(crate) sessions: Vec<FileSession>,
    pub(crate) root: PathBuf,
    pub(crate) token: Option<String>,
    pub(crate) default_key: Option<String>,
}

/// A [`SessionManager::resolve`] failure: the message plus, when this was
/// a failed *pull* (rather than a bad `file` argument or an unresolvable
/// default), how long the caller should wait before this session's next
/// watch tick (see [`do_pull`]) — `None` for every other kind of failure,
/// which carries no backoff information to act on.
#[derive(Debug)]
pub(crate) struct ResolveError {
    pub(crate) message: String,
    pub(crate) retry_after: Option<Duration>,
}

impl From<String> for ResolveError {
    fn from(message: String) -> Self {
        ResolveError {
            message,
            retry_after: None,
        }
    }
}

/// The `file`-argument resolution error's shared text (spec §14: must name
/// `figmog_open`/`figmog_files`).
const NO_DEFAULT_FILE_MSG: &str = "no file specified and no default mirrored file — pass a `file` argument, mirror one with figmog_open, or see figmog_files for the current list";

impl SessionManager {
    /// Get-or-create the session for `file_ref` (URL or bare key),
    /// deduped by key — never pulls: a freshly-created session is left
    /// exactly as [`open_session`] built it (empty unless the caller asked
    /// for `pull_now`). [`Self::resolve`]/`figmog_open` are what decide
    /// whether — and how many times — to actually pull.
    pub(crate) fn open(&mut self, file_ref: &str) -> Result<&mut FileSession, String> {
        let key = parse_file_ref(file_ref)
            .ok_or_else(|| format!("not a Figma file key or URL: {file_ref}"))?;
        if let Some(pos) = self.sessions.iter().position(|s| s.key == key) {
            return Ok(&mut self.sessions[pos]);
        }
        let session = open_session(&self.root, &key, self.token.as_deref(), false)?;
        self.sessions.push(session);
        Ok(self.sessions.last_mut().expect("just pushed"))
    }

    /// Spec §14's default-file rule, shared by [`Self::resolve`]'s omitted
    /// branch and [`Self::list`]'s `default` flag: the startup-established
    /// `default_key` if one was set, else the single mirrored file if
    /// exactly one exists, else `None` (ambiguous or nothing mirrored yet).
    pub(crate) fn effective_default_key(&self) -> Option<String> {
        if let Some(key) = &self.default_key {
            return Some(key.clone());
        }
        match self.sessions.len() {
            1 => Some(self.sessions[0].key.clone()),
            _ => None,
        }
    }

    /// Spec §14's `file`-argument resolution rule. Explicit `file`: that
    /// mirror, auto-opening it if unknown *or opened-but-never-
    /// successfully-mirrored* (a session whose first pull failed is
    /// retried here rather than served empty forever — see
    /// [`FileSession::mirrored`]), spending exactly one Tier-1 pull via
    /// [`do_pull`] (whose typed failure carries the retry-after wait the
    /// caller should push its own scheduling out by). Omitted:
    /// [`Self::effective_default_key`]'s session, or [`NO_DEFAULT_FILE_MSG`].
    /// Returns, alongside the session, the [`PullOutcome`] of a pull this
    /// call itself just performed (`None` if it didn't need to) — so a
    /// caller like `figmog_sync` can skip its own redundant pull.
    pub(crate) fn resolve(
        &mut self,
        file_arg: Option<&str>,
        interval: Duration,
    ) -> Result<(&mut FileSession, Option<PullOutcome>), ResolveError> {
        match file_arg {
            Some(f) => {
                let session = self.open(f)?;
                if session.mirrored {
                    Ok((session, None))
                } else {
                    // No user-facing geometry override reaches an implicit
                    // auto-open (only `figmog_open` carries one) — this
                    // just lets the mirror's stored setting, if any, drive
                    // the request (spec §4).
                    match do_pull(session, interval, false) {
                        Ok(outcome) => Ok((session, Some(outcome))),
                        Err((message, wait)) => Err(ResolveError {
                            message,
                            retry_after: Some(wait),
                        }),
                    }
                }
            }
            None => {
                let key = self
                    .effective_default_key()
                    .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?;
                let pos = self
                    .sessions
                    .iter()
                    .position(|s| s.key == key)
                    .ok_or_else(|| NO_DEFAULT_FILE_MSG.to_string())?;
                Ok((&mut self.sessions[pos], None))
            }
        }
    }

    /// `figmog_files`: every mirrored file, in open order, as `{key,
    /// name, version, nodes, last_synced, default}` — `default` per
    /// [`Self::effective_default_key`], not merely index 0 (spec §14: two
    /// auto-opened mirrors with no startup default have *no* default at
    /// all). Deterministic — plain `Vec` order, no `HashMap` involved.
    pub(crate) fn list(&mut self) -> Value {
        let default_key = self.effective_default_key();
        let rows: Vec<Value> = self
            .sessions
            .iter_mut()
            .map(|s| {
                let status = (s.dispatch)("figmog_status", &json!({})).ok();
                let (name, version, nodes, last_synced) = match status {
                    Some(ToolOutput::Json(v)) => (
                        v["name"].clone(),
                        v["version"].clone(),
                        v["nodes"].clone(),
                        v["synced_at_unix_ms"].clone(),
                    ),
                    _ => (Value::Null, Value::Null, Value::Null, Value::Null),
                };
                let is_default = default_key.as_deref() == Some(s.key.as_str());
                json!({
                    "key": s.key,
                    "name": name,
                    "version": version,
                    "nodes": nodes,
                    "last_synced": last_synced,
                    "default": is_default,
                })
            })
            .collect();
        json!(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiError;

    /// A scripted stand-in for a [`FileSession`] built without ever
    /// touching a real store — proves [`SessionManager`]'s routing/dedupe
    /// logic in isolation, per the brief's Step 1. `mirrored` matches what
    /// a real session freshly opened without a `pull_now` would have.
    fn scripted_session(key: &str, pull_calls: Rc<RefCell<u32>>) -> FileSession {
        scripted_session_with(key, pull_calls, false, |_| {
            Ok(PullOutcome {
                churn: Churn::default(),
                name: "Scripted".to_string(),
                version: "1".to_string(),
            })
        })
    }

    /// Like [`scripted_session`], but the caller controls `mirrored` at
    /// construction and scripts the pull outcome per call (`call_index`
    /// starting at 0) — used for the retry-after-failure and typed-error
    /// regression tests below.
    fn scripted_session_with(
        key: &str,
        pull_calls: Rc<RefCell<u32>>,
        mirrored: bool,
        script: impl Fn(u32) -> Result<PullOutcome, PullError> + 'static,
    ) -> FileSession {
        let watermark: WatermarkFn = Box::new(|| Some("t".to_string()));
        let pull: PullFn = {
            let pull_calls = pull_calls.clone();
            Box::new(move |_geometry: bool| {
                let call_index = *pull_calls.borrow();
                *pull_calls.borrow_mut() += 1;
                script(call_index)
            })
        };
        FileSession {
            key: key.to_string(),
            name: "Scripted".to_string(),
            dispatch: Box::new(|_name, _args| Ok(ToolOutput::Json(json!({})))),
            pull,
            watermark,
            proxy_cache: Box::new(|_upstream, name, _args| {
                Err(format!(
                    "scripted session has no store to proxy through: {name}"
                ))
            }),
            watcher: Watcher::new(None),
            backoff: BACKOFF_START,
            mirrored,
        }
    }

    fn empty_manager() -> SessionManager {
        SessionManager {
            sessions: Vec::new(),
            root: PathBuf::from("/nonexistent"),
            token: None,
            default_key: None,
        }
    }

    #[test]
    fn resolve_omitted_with_no_sessions_names_figmog_open_and_figmog_files() {
        let mut mgr = empty_manager();
        let err = mgr
            .resolve(None, Duration::from_secs(10))
            .map(|_| ())
            .unwrap_err();
        assert!(err.message.contains("figmog_open"), "{}", err.message);
        assert!(err.message.contains("figmog_files"), "{}", err.message);
    }

    #[test]
    fn resolve_omitted_errors_when_two_mirrors_were_auto_opened_with_no_startup_default() {
        // C1 regression: zero-file startup (no `default_key` ever set),
        // then two files get auto-opened via explicit-`file` tool calls —
        // an omitted `file` after that must still error naming
        // figmog_open/figmog_files, NOT silently fall back to whichever
        // was opened first.
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        mgr.sessions
            .push(scripted_session("keyB1234567890", calls.clone()));
        assert_eq!(mgr.default_key, None);
        let err = mgr
            .resolve(None, Duration::from_secs(10))
            .map(|_| ())
            .unwrap_err();
        assert!(err.message.contains("figmog_open"), "{}", err.message);
        assert!(err.message.contains("figmog_files"), "{}", err.message);
    }

    #[test]
    fn resolve_omitted_returns_the_startup_default_even_out_of_open_order() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        mgr.sessions
            .push(scripted_session("keyB1234567890", calls.clone()));
        // B was opened second but is the startup-established default —
        // resolve(None) must follow `default_key`, not `sessions[0]`.
        mgr.default_key = Some("keyB1234567890".to_string());
        let (session, outcome) = mgr.resolve(None, Duration::from_secs(10)).unwrap();
        assert_eq!(session.key, "keyB1234567890");
        assert!(outcome.is_none());
    }

    #[test]
    fn resolve_omitted_returns_the_single_mirrored_file_with_no_startup_default() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        let (session, _) = mgr.resolve(None, Duration::from_secs(10)).unwrap();
        assert_eq!(session.key, "keyA1234567890");
    }

    #[test]
    fn resolve_explicit_known_mirrored_key_never_pulls() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions.push(scripted_session_with(
            "flAtUnMfzvA5daBSTFQK35",
            calls.clone(),
            true,
            |_| unreachable!("must not pull an already-mirrored session"),
        ));
        let (session, outcome) = mgr
            .resolve(Some("flAtUnMfzvA5daBSTFQK35"), Duration::from_secs(10))
            .unwrap();
        assert_eq!(session.key, "flAtUnMfzvA5daBSTFQK35");
        assert!(outcome.is_none());
        assert_eq!(
            *calls.borrow(),
            0,
            "an already-mirrored session must not be re-pulled"
        );
    }

    #[test]
    fn resolve_explicit_unknown_key_dedupes_by_key_from_a_url_and_pulls_once() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions.push(scripted_session_with(
            "flAtUnMfzvA5daBSTFQK35",
            calls.clone(),
            false,
            |_| {
                Ok(PullOutcome {
                    churn: Churn::default(),
                    name: "F".to_string(),
                    version: "1".to_string(),
                })
            },
        ));
        // A full figma.com URL for the same key resolves to the existing
        // session rather than creating a second one (dedupe by parsed
        // key), and pulls it exactly once since it wasn't mirrored yet.
        let (session, outcome) = mgr
            .resolve(
                Some("https://www.figma.com/design/flAtUnMfzvA5daBSTFQK35/whatever"),
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(session.key, "flAtUnMfzvA5daBSTFQK35");
        assert!(session.mirrored);
        assert!(outcome.is_some());
        assert_eq!(mgr.sessions.len(), 1);
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn resolve_rejects_garbage_file_ref() {
        let mut mgr = empty_manager();
        let err = mgr
            .resolve(Some("not a key!"), Duration::from_secs(10))
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.message.contains("not a Figma file key or URL"),
            "{}",
            err.message
        );
        assert!(err.retry_after.is_none());
    }

    #[test]
    fn open_dedupes_repeated_calls_for_the_same_key() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        mgr.open("keyA1234567890").unwrap();
        mgr.open("keyA1234567890").unwrap();
        assert_eq!(mgr.sessions.len(), 1);
    }

    #[test]
    fn list_marks_default_via_effective_default_key_not_vec_order() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        mgr.sessions
            .push(scripted_session("keyB1234567890", calls.clone()));
        mgr.default_key = Some("keyB1234567890".to_string());
        let list = mgr.list();
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["key"], json!("keyA1234567890"));
        assert_eq!(rows[0]["default"], json!(false));
        assert_eq!(rows[1]["key"], json!("keyB1234567890"));
        assert_eq!(rows[1]["default"], json!(true));
    }

    #[test]
    fn list_marks_no_default_when_two_mirrors_and_no_startup_default() {
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions
            .push(scripted_session("keyA1234567890", calls.clone()));
        mgr.sessions
            .push(scripted_session("keyB1234567890", calls.clone()));
        let list = mgr.list();
        let rows = list.as_array().unwrap();
        assert!(rows.iter().all(|r| r["default"] == json!(false)));
    }

    #[test]
    fn resolve_retries_the_pull_for_a_session_whose_first_pull_failed() {
        // I3 regression: `open()` pushes the session before any pull can
        // succeed or fail — a failed auto-open must not poison the key
        // into permanent empty results. `mirrored` starts `false` (as a
        // real failed-open session would), and the scripted pull fails on
        // the first call, succeeds on the second.
        let mut mgr = empty_manager();
        let calls = Rc::new(RefCell::new(0));
        mgr.sessions.push(scripted_session_with(
            "keyA1234567890",
            calls.clone(),
            false,
            |call_index| {
                if call_index == 0 {
                    Err(PullError::from("network down".to_string()))
                } else {
                    Ok(PullOutcome {
                        churn: Churn::default(),
                        name: "Recovered".to_string(),
                        version: "2".to_string(),
                    })
                }
            },
        ));

        let err = mgr
            .resolve(Some("keyA1234567890"), Duration::from_secs(10))
            .map(|_| ())
            .unwrap_err();
        assert!(!err.message.is_empty());
        assert!(err.retry_after.is_some());
        assert_eq!(
            mgr.sessions.len(),
            1,
            "the poisoned session is kept, not evicted"
        );
        assert!(!mgr.sessions[0].mirrored);

        // A second resolve for the same key retries the pull (not treated
        // as already-mirrored) and this time succeeds.
        let (session, outcome) = mgr
            .resolve(Some("keyA1234567890"), Duration::from_secs(10))
            .unwrap();
        assert!(session.mirrored);
        assert_eq!(outcome.unwrap().name, "Recovered");
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn typed_pull_error_survives_the_closure_boundary_for_rate_limit_backoff() {
        // C2 regression: a `PullError::Api(RateLimited { .. })` returned
        // from the boxed `pull` closure must still be *typed* by the time
        // `do_pull` sees it — not degraded to a plain string — so
        // `pull_failure_wait` can honor `Retry-After` instead of falling
        // back to plain exponential backoff.
        let mut backoff = BACKOFF_START;
        let mut session =
            scripted_session_with("keyA1234567890", Rc::new(RefCell::new(0)), false, |_| {
                Err(PullError::Api(ApiError::RateLimited {
                    retry_after: Duration::from_secs(90),
                }))
            });
        session.backoff = backoff;

        let (message, wait) = do_pull(&mut session, Duration::from_secs(10), false).unwrap_err();
        assert!(message.contains("retry after"), "{message}");
        // `pull_failure_wait` honors Retry-After for a rate limit
        // regardless of the configured interval, and does NOT touch the
        // exponential-backoff budget — proving the typed variant, not a
        // stringly-typed fallback, reached `pull_failure_wait`.
        assert_eq!(wait, Duration::from_secs(90));
        assert_eq!(session.backoff, BACKOFF_START);
        let _ = &mut backoff;
    }

    // ---- sticky vector geometry (v0.0.2 spec §4) ----

    /// [`do_pull`]'s `geometry` argument must reach the session's own
    /// `pull` closure unchanged — the plumbing every re-pull call site
    /// (`figmog_open`'s explicit arg, or `false` from every other caller:
    /// startup, watch tick, `figmog_sync`, auto-open) relies on. The
    /// closure's own decision to union this with the stored flag
    /// (`store::effective_geometry`) is proven separately and purely at
    /// the store/api layer — this test only proves the *argument*
    /// threading, at the one layer (`FileSession`/`SessionManager`) that
    /// can't be exercised without a real store or network.
    #[test]
    fn do_pull_forwards_its_geometry_argument_to_the_session_pull_closure() {
        let seen: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let pull: PullFn = {
            let seen = seen.clone();
            Box::new(move |geometry: bool| {
                seen.borrow_mut().push(geometry);
                Ok(PullOutcome {
                    churn: Churn::default(),
                    name: "Scripted".to_string(),
                    version: "1".to_string(),
                })
            })
        };
        let mut session = FileSession {
            key: "keyA1234567890".to_string(),
            name: "Scripted".to_string(),
            dispatch: Box::new(|_name, _args| Ok(ToolOutput::Json(json!({})))),
            pull,
            watermark: Box::new(|| Some("t".to_string())),
            proxy_cache: Box::new(|_upstream, name, _args| {
                Err(format!(
                    "scripted session has no store to proxy through: {name}"
                ))
            }),
            watcher: Watcher::new(None),
            backoff: BACKOFF_START,
            mirrored: false,
        };

        do_pull(&mut session, Duration::from_secs(10), true).unwrap();
        do_pull(&mut session, Duration::from_secs(10), false).unwrap();

        assert_eq!(*seen.borrow(), vec![true, false]);
    }
}
