//! `figmog pull`: fetch-flatten-sync-evict, the typed [`PullError`] it can
//! fail with, and `.figmog/current` (the "which file did we last touch"
//! pointer every other command's [`super::resolve_db`] reads).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api::{ApiError, FigmaApi, UreqApi};
use crate::flatten::flatten_file;
use crate::ident::parse_file_ref;
use crate::store::{self, Churn, collect_sweepable, collect_variable_ids, sync};
use crate::watch::BACKOFF_CAP;

use super::{CURRENT_FILE, Db, open_store_checked, write_json};

/// Errors from [`do_pull`]: either a typed API failure (so callers can act
/// on rate limits) or any other pull-mechanics failure. `Display` matches
/// the plain-string messages `do_pull` used to produce, so `cmd_pull`'s
/// user-facing errors are unchanged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PullError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("{0}")]
    Other(String),
}

impl From<String> for PullError {
    fn from(s: String) -> Self {
        PullError::Other(s)
    }
}

pub(super) fn cmd_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
    geometry: bool,
) -> Result<(), String> {
    let (churn, _name, _version) = do_pull(db, file, from_file, fresh, geometry).map_err(|e| {
        let message = e.to_string();
        // `pull` is a writer (spec §1: it stays direct-open, never routed
        // over the socket), so a running `figmog serve` holding the same
        // store's single-writer lock is the single most common way this
        // fails while serve is up — point at the one escape hatch that
        // still works in that situation (`figmog call figmog_sync` reaches
        // the *owning* process over the socket) rather than leaving the
        // generic "is figmog serve running?" message to speak for itself.
        // Scoped to exactly this message (not every `do_pull` failure) so
        // an unrelated error (bad JSON, a real network failure, ...) isn't
        // given a hint that doesn't apply to it.
        if message == super::STORE_LOCKED_MSG {
            format!("{message} — or ask the running serve: figmog call figmog_sync")
        } else {
            message
        }
    })?;
    print_churn(&churn)
}

/// The pull mechanics without any printing. `.figmog/current` is written
/// only once the sync below has actually happened, so a failed pull never
/// repoints later commands at a nonexistent mirror.
///
/// `geometry` is this call's own override (spec §4 stickiness): the
/// network fetch requests `?geometry=paths` when `geometry` is set *or*
/// the mirror's already-stored config has it on
/// (`store::effective_geometry`); the resulting flag is persisted back via
/// `store::upsert_mirror_config` regardless of path, so it survives to
/// drive the next pull even for `--from-file` (which never touches the
/// network, and so never reads this flag for a fetch parameter at all —
/// see the `from_file` match arm below).
pub(super) fn do_pull(
    db: &Db,
    file: Option<String>,
    from_file: Option<PathBuf>,
    fresh: bool,
    geometry: bool,
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
            // Peek the mirror's stored geometry setting (spec §4) *before*
            // fetching, so a plain re-pull of a mirror that was ever
            // pulled with `--geometry` keeps asking for it. `--fresh`
            // wipes the store below and never consults it (the documented
            // way to turn geometry back off), so this never opens a store
            // that's about to be discarded — and a first-ever pull (no
            // store on disk yet) never opens one at all, so a token
            // failure above still leaves no store trace (unchanged from
            // pre-v0.0.2 — see `failed_pull_does_not_persist_current_or_
            // create_store`). When a store does exist, this is a second,
            // short-lived open dropped before the real one further down —
            // fjall allows only one open handle per store per process at a
            // time, not one ever; dropping releases its lock.
            let stored_geometry = if fresh || !db.path.exists() {
                false
            } else {
                open_store_checked(|| crate::open_store!(&db.path))
                    .map(|st| st.rtx(|(.., mirror_config, _)| store::read_geometry(&mirror_config)))
                    .unwrap_or(false)
            };
            let request_geometry = store::effective_geometry(geometry, stored_geometry);
            let api = UreqApi::new(token);
            let resp = api.file(&key, request_geometry)?;
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
    let mut prior: BTreeSet<crate::model::Id> =
        st.rtx(|((nodes, ..), components, component_sets, styles, ..)| {
            collect_sweepable(&nodes, &components, &component_sets, &styles)
        });
    if let Some(v) = &vars_resp {
        let var_recs = crate::vars::parse_variables_export(v).map_err(|e| e.to_string())?;
        flattened.recs.extend(var_recs);
        let stored_var_ids = st.rtx(
            |(_, _, _, _, variables, variable_collections, _, _, _, _)| {
                collect_variable_ids(&variables, &variable_collections)
            },
        );
        prior.extend(stored_var_ids);
    }
    let prior_version =
        st.rtx(|(_, _, _, _, _, _, meta, _, _, _)| meta.get(&0).map(|m| m.version.clone()));
    let churn = sync(&mut st, &prior, &flattened, now_ms());

    // Every caller of `do_pull` (`pull` and `figmog call figmog_sync`) goes
    // through here, so eviction lives here rather than duplicated at each
    // call site (build design §12: a
    // version-changing pull sweeps stale `proxy_cache` rows; v0.0.2 spec §5
    // extends this to stale `images` rows the same way). `figmog serve`'s
    // own pull paths don't call `do_pull` — they keep their own inline
    // eviction blocks, since they already hold `st` open and re-opening it
    // here would hit the same single-open-per-process wall `figmog call
    // figmog_sync` used to.
    if prior_version.as_deref() != Some(flattened.file.version.as_str()) {
        let mut stale = st.rtx(|(_, _, _, _, _, _, _, cache, _, images)| {
            let mut stale = crate::store::stale_cache_ids(&cache, &flattened.file.version);
            stale.extend(crate::store::stale_image_ids(
                &images,
                &flattened.file.version,
            ));
            stale
        });
        if !stale.is_empty() {
            stale.sort();
            crate::store::evict_stale_cache(&mut st, &stale);
        }
    }

    // Persist the sticky geometry flag (spec §4): this call's own flag OR
    // whatever's already stored in *this* handle (re-read here rather than
    // reusing `stored_geometry` above — that value came from a separate,
    // now-dropped peek on the network path, and doesn't exist at all on
    // the `--from-file` path, which still must persist stickiness even
    // though it never requested anything over the network — see this
    // function's doc comment). `--fresh` already wiped any prior row, so
    // this store's own current read is `false` there, giving exactly
    // `geometry` as the persisted value, same as the fetch decision above.
    let stored_now = st.rtx(|(.., mirror_config, _)| store::read_geometry(&mirror_config));
    store::upsert_mirror_config(&mut st, store::effective_geometry(geometry, stored_now));

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
/// discipline the [`Watcher`](crate::watch::Watcher) uses for Tier-3 meta
/// failures.
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

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub(crate) fn write_current(key: &str) -> Result<(), String> {
    std::fs::create_dir_all(".figmog").map_err(|e| e.to_string())?;
    std::fs::write(CURRENT_FILE, key).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::BACKOFF_START;

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
