//! Cached-proxy CLI parity: `figmog tools`, `figmog call`, and
//! `figmog import-variables` — every tool `figmog serve` would expose,
//! reachable without an MCP client.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::dispatch;
use crate::model::Id;
use crate::proxy;
use crate::upstream::{HttpUpstream, UpstreamMcp};

use super::pull::do_pull;
use super::{Db, open_store_checked, write_json};

pub(super) fn cmd_import_variables(db: &Db, path: PathBuf) -> Result<(), String> {
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
/// mirror — local tools always, upstream tools when reachable. Never opens
/// the store (spec §4 debt item M5): dispatched in `super::dispatch`
/// before `resolve_db`, so it works with no established mirror at all.
pub(super) fn cmd_tools(upstream_url: String, no_upstream: bool) -> Result<(), String> {
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
pub(super) fn cmd_call(
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
        // `geometry: false` here means "no new override" — `do_pull`
        // internally unions this with whatever's already stored (spec §4
        // stickiness), so a plain `figmog_sync` still re-requests geometry
        // for a mirror that was ever pulled with `--geometry`.
        let result = do_pull(db, None, None, false, false)
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
            st.rtx(|(_, _, _, _, _, _, meta, cache, _)| {
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
